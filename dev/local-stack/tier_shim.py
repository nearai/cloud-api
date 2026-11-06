#!/usr/bin/env python3
"""Per-tier backend shim for the local dev stack.

Sits between cloud-api and a single shared llama.cpp server, playing the role
one SGLang fleet plays in prod:

  * GET  /v1/models           -> advertises the canonical model id
  * POST /v1/tokenize         -> REAL token counts (proxied to llama-server's
                                 native /tokenize), vLLM/SGLang response shape
                                 {"count": N} — this is what drives cloud-api's
                                 exact-count boundary refinement
  * POST /v1/chat/completions -> enforces THIS TIER's context window with the
                                 SGLang-phrased 400 (so cloud-api's
                                 context-400 fall-through matcher fires exactly
                                 as in prod), otherwise proxies to the engine.
                                 Non-streaming responses get the tier tag
                                 appended to `.model` ("<id>+base"/"<id>+long")
                                 so curl output shows which tier served.

Saturation drill: `touch <state-dir>/SATURATE-<tier>` flips this shim to 503
("queue full"), emulating SGLang's bounded queue — cloud-api then walks its
fallback chain. Remove the file to recover.

Stdlib only — no venv needed.
"""

import argparse
import json
import os
import sys
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None


def log(msg: str) -> None:
    print(f"[{ARGS.tier}] {msg}", flush=True)


def engine(path: str, body: dict | None = None, raw: bytes | None = None,
           headers: dict | None = None, stream: bool = False):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(
        f"{ARGS.engine}{path}",
        data=data,
        headers={"Content-Type": "application/json", **(headers or {})},
        method="POST" if data is not None else "GET",
    )
    return urllib.request.urlopen(req, timeout=ARGS.engine_timeout)


def count_tokens(text: str) -> int:
    with engine("/tokenize", body={"content": text}) as r:
        return len(json.load(r).get("tokens", []))


def request_text(payload: dict) -> str:
    parts = []
    for m in payload.get("messages", []):
        c = m.get("content")
        if isinstance(c, str):
            parts.append(c)
        elif isinstance(c, list):
            parts.extend(p.get("text", "") for p in c if isinstance(p, dict))
    if payload.get("tools"):
        parts.append(json.dumps(payload["tools"]))
    return "\n".join(parts)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):  # quiet the default access log; we log ourselves
        pass

    def _json(self, code: int, obj: dict):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self) -> bytes:
        return self.rfile.read(int(self.headers.get("Content-Length", 0)))

    def do_GET(self):
        if self.path == "/v1/models":
            self._json(200, {"object": "list",
                             "data": [{"id": ARGS.model_id, "object": "model",
                                       "owned_by": "nearai"}]})
        elif self.path in ("/", "/health", "/healthz"):
            self._json(200, {"status": "ok", "tier": ARGS.tier})
        else:
            self._json(404, {"error": {"message": f"no route {self.path}"}})

    def do_POST(self):
        try:
            if self.path == "/v1/tokenize":
                payload = json.loads(self._read_body())
                n = count_tokens(payload.get("prompt") or payload.get("content") or "")
                log(f"POST /v1/tokenize -> count={n}")
                self._json(200, {"count": n})
            elif self.path == "/v1/chat/completions":
                self.handle_completion()
            else:
                self._json(404, {"error": {"message": f"no route {self.path}"}})
        except Exception as e:  # keep the shim alive; surface as a 500
            log(f"ERROR {self.path}: {e}")
            try:
                self._json(500, {"error": {"message": str(e)}})
            except Exception:
                pass

    def handle_completion(self):
        payload = json.loads(self._read_body())

        if os.path.exists(ARGS.saturate_file):
            log("POST /v1/chat/completions -> 503 (SATURATED)")
            self._json(503, {"error": {"message": "queue full", "type": "server_error"}})
            return

        # Enforce this tier's window like SGLang does, with its phrasing so
        # cloud-api's fall-through matcher behaves exactly as in prod.
        n_input = count_tokens(request_text(payload))
        max_new = int(payload.get("max_tokens") or payload.get("max_completion_tokens") or 0)
        if n_input + max_new > ARGS.ctx_window:
            log(f"POST /v1/chat/completions -> 400 context ({n_input}+{max_new} > {ARGS.ctx_window})")
            self._json(400, {"error": {
                "message": (f"This model's maximum context length is {ARGS.ctx_window} tokens. "
                            f"However, you requested {n_input + max_new} tokens "
                            f"({n_input} in the messages, {max_new} in the completion). "
                            f"Please reduce the length of the messages or completion."),
                "type": "invalid_request_error"}})
            return

        stream = bool(payload.get("stream"))
        upstream = dict(payload)
        upstream["model"] = ARGS.engine_model
        with engine("/v1/chat/completions", body=upstream) as r:
            if stream:
                # SSE passthrough (tier tag only on non-streaming .model).
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                sent = 0
                while chunk := r.read(8192):
                    self.wfile.write(chunk)
                    sent += len(chunk)
                log(f"POST /v1/chat/completions [stream] -> 200 ({n_input} tok in, {sent}B out)")
            else:
                resp = json.load(r)
                resp["model"] = f"{ARGS.model_id}+{ARGS.tier}"
                log(f"POST /v1/chat/completions -> 200 ({n_input} tok in)")
                self._json(200, resp)


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--tier", required=True, choices=["base", "long"])
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--ctx-window", type=int, required=True)
    p.add_argument("--model-id", required=True)
    p.add_argument("--engine", default="http://127.0.0.1:18090")
    p.add_argument("--engine-model", default="local")
    p.add_argument("--engine-timeout", type=int, default=120)
    p.add_argument("--state-dir", default=os.path.dirname(os.path.abspath(__file__)))
    ARGS = p.parse_args()
    ARGS.saturate_file = os.path.join(ARGS.state_dir, f"SATURATE-{ARGS.tier}")

    srv = ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler)
    log(f"tier shim on :{ARGS.port} ctx={ARGS.ctx_window} engine={ARGS.engine} "
        f"(saturate file: {ARGS.saturate_file})")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
