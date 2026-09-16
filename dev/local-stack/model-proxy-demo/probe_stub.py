#!/usr/bin/env python3
"""Probe endpoints for the model-proxy control-plane demo.

Emulates the GLM-5.2 TP8 host's two discovery endpoints:
  :18000 -> /v1/models advertising the canonical id  (the serving probe)
  :18001 -> /v1/models advertising the synthetic -long id, HEALTH-GATED the
            same way the PR-#129 nginx stub is (auth_request to the engine):
            when ./ENGINE_DOWN exists, both answer 5xx — engine dead.

Stdlib only; both servers run in one process.
"""

import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
GATE_FILE = os.path.join(HERE, "ENGINE_DOWN")


def make_handler(model_id: str):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):
            print(f"[{model_id}] {fmt % args}", flush=True)

        def do_GET(self):
            if os.path.exists(GATE_FILE):
                body = json.dumps({"error": {"message": "engine down"}}).encode()
                self.send_response(502)
            elif self.path == "/v1/models":
                body = json.dumps(
                    {"object": "list",
                     "data": [{"id": model_id, "object": "model", "owned_by": "nearai"}]}
                ).encode()
                self.send_response(200)
            else:
                body = b"{}"
                self.send_response(404)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    return Handler


def serve(port: int, model_id: str):
    ThreadingHTTPServer(("127.0.0.1", port), make_handler(model_id)).serve_forever()


if __name__ == "__main__":
    threading.Thread(target=serve, args=(18000, "z-ai/glm-5.2-e2e"), daemon=True).start()
    print("probe stubs on :18000 (canonical) and :18001 (synthetic -long)", flush=True)
    serve(18001, "z-ai/glm-5.2-e2e-long")
