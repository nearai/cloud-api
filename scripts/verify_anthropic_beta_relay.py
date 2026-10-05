#!/usr/bin/env python3
"""Opt-in synthetic native Messages rollout checks; see verify_anthropic_beta_relay.md.

Uses only the standard library. Never prints credentials, response bodies, or
request IDs. Exit 1 means a probe failed; exit 2 means invalid configuration.
"""

from __future__ import annotations

import argparse
import http.client
import json
import math
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from test_anthropic_cache_walk import ANTHROPIC_BETA

STAGING_URL = "https://cloud-stg-api.near.ai"
TEST_BETA = "not-a-real-beta-2099-01-01"
MAX_RESPONSE_BYTES = 2 * 1024 * 1024


class CheckFailed(Exception):
    """A fixed diagnostic that is safe to print (never include upstream text)."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise CheckFailed(message)


def object_json(raw: bytes | str) -> dict:
    try:
        value = json.loads(raw)
    except (ValueError, UnicodeError):
        raise CheckFailed("invalid JSON") from None
    require(isinstance(value, dict), "expected a JSON object")
    return value


def token_count(usage: dict, key: str) -> int:
    value = usage.get(key)
    require(type(value) is int and value >= 0, "missing or invalid usage count")
    return value


def input_usage(usage: dict) -> int:
    require(isinstance(usage, dict), "missing usage object")
    total = token_count(usage, "input_tokens")
    for key in ("cache_read_input_tokens", "cache_creation_input_tokens"):
        if key in usage:
            total += token_count(usage, key)
    require(total > 0, "empty input usage")
    return total


def stop_reason(value: object) -> None:
    require(value in ("end_turn", "max_tokens"), "missing or unsuccessful stop reason")


@dataclass
class Reply:
    status: int
    headers: dict[str, str]
    body: bytes


def validate_message(reply: Reply) -> None:
    value = object_json(reply.body)
    require(
        value.get("type") == "message" and value.get("role") == "assistant",
        "expected an assistant message",
    )
    require(
        isinstance(value.get("id"), str) and bool(value["id"]), "missing message ID"
    )
    content = value.get("content")
    require(
        isinstance(content, list)
        and any(
            isinstance(block, dict)
            and block.get("type") == "text"
            and isinstance(block.get("text"), str)
            and bool(block["text"])
            for block in content
        ),
        "missing text output",
    )
    usage = value.get("usage")
    input_usage(usage)
    require(token_count(usage, "output_tokens") > 0, "empty output usage")
    stop_reason(value.get("stop_reason"))


def validate_stream(reply: Reply) -> None:
    try:
        text = reply.body.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
    except UnicodeError:
        raise CheckFailed("invalid SSE encoding") from None
    frames = text.split("\n\n")
    require(not frames[-1].strip(), "incomplete SSE frame")
    started = stopped = finished = output_seen = False
    open_blocks = set()
    final_output = 0
    for frame in frames[:-1]:
        data, event_name = [], None
        for line in frame.split("\n"):
            if line.startswith(":"):
                continue
            field, _, value = line.partition(":")
            value = value.removeprefix(" ")
            if field == "data":
                data.append(value)
            elif field == "event":
                event_name = value
        if not data:
            continue
        event = object_json("\n".join(data))
        kind = event.get("type")
        require(isinstance(kind, str), "missing SSE event type")
        require(event_name is None or event_name == kind, "SSE event type mismatch")
        require(kind != "error", "SSE error event")
        if kind == "ping":
            continue
        require(not stopped, "SSE data after message_stop")
        if kind == "message_start":
            require(not started, "duplicate message_start")
            message = event.get("message")
            require(isinstance(message, dict), "missing message_start object")
            require(
                message.get("type") == "message" and message.get("role") == "assistant",
                "invalid message_start object",
            )
            require(
                isinstance(message.get("id"), str) and bool(message["id"]),
                "missing message ID",
            )
            input_usage(message.get("usage"))
            final_output = token_count(message["usage"], "output_tokens")
            started = True
        else:
            require(started, "SSE data before message_start")
            if kind == "message_delta":
                require(not open_blocks, "unclosed content block")
                usage, delta = event.get("usage"), event.get("delta")
                require(
                    isinstance(usage, dict) and isinstance(delta, dict),
                    "missing final usage or delta",
                )
                count = token_count(usage, "output_tokens")
                require(count >= final_output, "output usage decreased")
                final_output = count
                stop_reason(delta.get("stop_reason"))
                finished = True
            elif kind == "message_stop":
                require(
                    finished and final_output > 0 and output_seen,
                    "message_stop without final usage and text output",
                )
                stopped = True
            elif kind in ("content_block_start", "content_block_delta"):
                require(not finished, "content after final message_delta")
                index = event.get("index")
                require(
                    type(index) is int and index >= 0, "invalid content block index"
                )
                if kind == "content_block_start":
                    require(index not in open_blocks, "duplicate content block start")
                    open_blocks.add(index)
                else:
                    require(index in open_blocks, "content delta without block start")
                block = event.get(
                    "content_block" if kind == "content_block_start" else "delta"
                )
                require(isinstance(block, dict), "invalid content block")
                output_seen |= isinstance(block.get("text"), str) and bool(
                    block["text"]
                )
            elif kind == "content_block_stop":
                index = event.get("index")
                require(
                    type(index) is int and index in open_blocks,
                    "content stop without block start",
                )
                open_blocks.remove(index)
    require(stopped, "missing message_stop")


@dataclass
class Probe:
    label: str
    path: str
    betas: str
    body: dict
    kind: str
    error_text: str = ""


def validate(reply: Reply, probe: Probe) -> None:
    is_error = probe.kind in ("upstream_error", "router_error")
    require(reply.status == (400 if is_error else 200), "unexpected HTTP status")
    content_type = reply.headers.get("content-type", "").split(";")[0].lower().strip()
    require(
        content_type
        == ("text/event-stream" if probe.kind == "stream" else "application/json"),
        "unexpected content type",
    )
    upstream = bool(reply.headers.get("request-id"))
    require(upstream == (probe.kind != "router_error"), "wrong error/response origin")
    if is_error:
        value = object_json(reply.body)
        error = value.get("error")
        require(
            value.get("type") == "error" and isinstance(error, dict),
            "invalid error envelope",
        )
        require(error.get("type") == "invalid_request_error", "wrong error category")
        require(
            isinstance(error.get("message"), str)
            and probe.error_text in error["message"],
            "wrong error reason",
        )
        require(
            not reply.headers.get("inference-id"), "error unexpectedly has inference ID"
        )
    elif probe.kind == "count":
        require(
            token_count(object_json(reply.body), "input_tokens") > 0,
            "empty token count",
        )
    else:
        require(bool(reply.headers.get("inference-id")), "missing inference ID")
        if probe.kind == "stream":
            validate_stream(reply)
        else:
            validate_message(reply)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def request(api_url: str, key: str, timeout: float, probe: Probe) -> Reply:
    req = urllib.request.Request(
        api_url + probe.path + "?beta=true",
        data=json.dumps(probe.body).encode(),
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "anthropic-version": "2023-06-01",
            "anthropic-beta": probe.betas,
        },
        method="POST",
    )
    try:
        response = urllib.request.build_opener(NoRedirect()).open(req, timeout=timeout)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        body = response.read(MAX_RESPONSE_BYTES + 1)
        require(len(body) <= MAX_RESPONSE_BYTES, "response exceeded size limit")
        return Reply(
            response.code, {k.lower(): v for k, v in response.headers.items()}, body
        )


def probes(model: str, phase: str) -> list[Probe]:
    count_body = {
        "model": model,
        "messages": [{"role": "user", "content": "Reply with OK."}],
    }
    body = {**count_body, "max_tokens": 32}
    result = [
        Probe("full header: message", "/v1/messages", ANTHROPIC_BETA, body, "message"),
        Probe(
            "full header: stream",
            "/v1/messages",
            ANTHROPIC_BETA,
            {**body, "stream": True},
            "stream",
        ),
        Probe(
            "full header: count",
            "/v1/messages/count_tokens",
            ANTHROPIC_BETA,
            count_body,
            "count",
        ),
    ]
    for path, payload in (
        ("/v1/messages", body),
        ("/v1/messages/count_tokens", count_body),
    ):
        if phase == "denylist":
            for beta in (TEST_BETA, TEST_BETA.upper()):
                case = "lowercase" if beta == TEST_BETA else "uppercase"
                result.append(
                    Probe(
                        f"denylist {case}: {path}",
                        path,
                        f"{ANTHROPIC_BETA},{beta}",
                        payload,
                        "router_error",
                        "disabled on this endpoint by operator policy",
                    )
                )
        else:
            result.extend(
                [
                    Probe(
                        f"unknown beta: {path}",
                        path,
                        f"{ANTHROPIC_BETA},{TEST_BETA}",
                        payload,
                        "upstream_error",
                        "Unexpected value",
                    ),
                    Probe(
                        f"malformed beta: {path}",
                        path,
                        "malformed beta",
                        payload,
                        "router_error",
                        "is malformed",
                    ),
                ]
            )
    result.extend(
        [
            Probe(
                "fast header only",
                "/v1/messages",
                "fast-mode-2026-02-01",
                body,
                "message",
            ),
            Probe(
                "fast body rejected",
                "/v1/messages",
                "fast-mode-2026-02-01",
                {**body, "speed": "fast"},
                "router_error",
                "speed=fast",
            ),
        ]
    )
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--api-url", default=STAGING_URL)
    parser.add_argument(
        "--api-key-file", type=Path, help="otherwise read API_KEY from the environment"
    )
    parser.add_argument("--model", default="anthropic/claude-sonnet-4-6")
    parser.add_argument("--phase", choices=("relay", "denylist"), default="relay")
    parser.add_argument("--timeout", type=float, default=45)
    parser.add_argument("--allow-production", action="store_true")
    args = parser.parse_args(argv)
    url = urllib.parse.urlsplit(args.api_url)
    if (
        url.scheme != "https"
        or not url.hostname
        or url.username
        or url.password
        or url.query
        or url.fragment
        or url.path not in ("", "/")
    ):
        parser.error(
            "--api-url must be an HTTPS origin without credentials, path, or query"
        )
    if url.hostname == "cloud-api.near.ai" and not args.allow_production:
        parser.error("production probes require --allow-production")
    if args.phase == "denylist" and args.api_url.rstrip("/") != STAGING_URL:
        parser.error("the temporary denylist check is staging-only")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be positive and finite")
    try:
        key = (
            args.api_key_file.read_text()
            if args.api_key_file
            else os.environ.get("API_KEY", "")
        ).strip()
    except OSError:
        parser.error("could not read API key file")
    if not key or any(char.isspace() for char in key):
        parser.error("a non-empty API key without whitespace is required")
    failures = 0
    checks = probes(args.model, args.phase)
    for probe in checks:
        status = 0
        try:
            reply = request(args.api_url.rstrip("/"), key, args.timeout, probe)
            status = reply.status
            validate(reply, probe)
        except (CheckFailed, OSError, http.client.HTTPException, ValueError) as error:
            failures += 1
            reason = (
                str(error) if isinstance(error, CheckFailed) else "transport failure"
            )
            print(f"FAIL {probe.label}: http={status} {reason}")
        else:
            print(f"PASS {probe.label}: http={status}")
    print(f"{len(checks) - failures}/{len(checks)} probes passed ({args.phase})")
    return int(failures != 0)


if __name__ == "__main__":
    sys.exit(main())
