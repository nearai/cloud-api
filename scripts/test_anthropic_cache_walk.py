#!/usr/bin/env python3
"""Run a synthetic four-turn Anthropic prompt-cache walk against staging.

This is an opt-in live test. It never prints request or response bodies and it
refuses the production Cloud API unless ALLOW_PRODUCTION=1 is set explicitly.

Examples:
    API_KEY=... python3 scripts/test_anthropic_cache_walk.py
    API_KEY=... PROTOCOL=messages python3 scripts/test_anthropic_cache_walk.py
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request
from typing import Any


API_URL = os.environ.get("API_URL", "https://cloud-stg-api.near.ai").rstrip("/")
API_KEY = os.environ.get("API_KEY", "")
MODEL = os.environ.get("MODEL", "anthropic/claude-sonnet-4-6")
PROTOCOL = os.environ.get("PROTOCOL", "chat")
DELAY_SECONDS = float(os.environ.get("CACHE_WALK_DELAY_SECONDS", "2"))
TIMEOUT_SECONDS = float(os.environ.get("CACHE_WALK_TIMEOUT_SECONDS", "90"))
ANTHROPIC_VERSION = "2023-06-01"
ANTHROPIC_BETA = os.environ.get(
    "ANTHROPIC_BETA",
    ",".join(
        [
            "claude-code-20250219",
            "interleaved-thinking-2025-05-14",
            "thinking-token-count-2026-05-13",
            "context-management-2025-06-27",
            "prompt-caching-scope-2026-01-05",
            "advisor-tool-2026-03-01",
            "effort-2025-11-24",
            "structured-outputs-2025-12-15",
        ]
    ),
)

CACHE_CONTROL = {"type": "ephemeral"}
SYSTEM_TEXT = (
    "Synthetic cache-walk instruction. Use only the supplied synthetic lookup results. "
    "Do not infer external facts. Keep the final answer to one word. "
) * 80

TOOL = {
    "name": "lookup",
    "description": "Return a synthetic record",
    "input_schema": {
        "type": "object",
        "properties": {"record": {"type": "integer"}},
        "required": ["record"],
    },
}


def fail(message: str) -> None:
    print(f"FAIL: {message}", file=sys.stderr)
    raise SystemExit(1)


def chat_messages(turn: int) -> list[dict[str, Any]]:
    messages: list[dict[str, Any]] = [
        {
            "role": "system",
            "content": [
                {
                    "type": "text",
                    "text": SYSTEM_TEXT,
                    "cache_control": CACHE_CONTROL,
                }
            ],
        },
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "Fetch synthetic records 101 through 104 one at a time.",
                }
            ],
        },
    ]
    for index in range(1, turn + 1):
        call_id = f"toolu_cache_walk_{index}"
        messages.extend(
            [
                {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": "lookup",
                                "arguments": json.dumps({"record": 100 + index}),
                            },
                        }
                    ],
                },
                {
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": [
                        {
                            "type": "text",
                            "text": f"synthetic record {100 + index}: ok",
                            **(
                                {"cache_control": CACHE_CONTROL}
                                if index == turn
                                else {}
                            ),
                        }
                    ],
                },
            ]
        )
    return messages


def native_messages(turn: int) -> list[dict[str, Any]]:
    messages: list[dict[str, Any]] = [
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "Fetch synthetic records 101 through 104 one at a time.",
                }
            ],
        }
    ]
    for index in range(1, turn + 1):
        call_id = f"toolu_cache_walk_{index}"
        messages.extend(
            [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": call_id,
                            "name": "lookup",
                            "input": {"record": 100 + index},
                        }
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": f"synthetic record {100 + index}: ok",
                            **(
                                {"cache_control": CACHE_CONTROL}
                                if index == turn
                                else {}
                            ),
                        }
                    ],
                },
            ]
        )
    return messages


def build_request(turn: int) -> tuple[str, dict[str, Any]]:
    if PROTOCOL == "chat":
        return (
            "/v1/chat/completions",
            {
                "model": MODEL,
                "messages": chat_messages(turn),
                "max_tokens": 1,
                "tools": [
                    {
                        "type": "function",
                        "function": {
                            "name": TOOL["name"],
                            "description": TOOL["description"],
                            "parameters": TOOL["input_schema"],
                        },
                    }
                ],
            },
        )
    if PROTOCOL == "messages":
        return (
            "/v1/messages",
            {
                "model": MODEL,
                "system": [
                    {
                        "type": "text",
                        "text": SYSTEM_TEXT,
                        "cache_control": CACHE_CONTROL,
                    }
                ],
                "messages": native_messages(turn),
                "max_tokens": 1,
                "tools": [TOOL],
            },
        )
    fail("PROTOCOL must be either 'chat' or 'messages'")
    raise AssertionError("unreachable")


def usage_counts(payload: dict[str, Any]) -> tuple[int, int, int]:
    usage = payload.get("usage") or {}
    if PROTOCOL == "messages":
        return (
            int(usage.get("input_tokens", 0)),
            int(usage.get("cache_read_input_tokens", 0)),
            int(usage.get("cache_creation_input_tokens", 0)),
        )
    details = usage.get("prompt_tokens_details") or {}
    return (
        int(usage.get("prompt_tokens", 0)),
        int(details.get("cached_tokens", 0)),
        int(details.get("cache_creation_tokens", 0)),
    )


def request_json(
    path: str, body: dict[str, Any], *, use_anthropic_auth: bool = False
) -> dict[str, Any]:
    headers = {"Content-Type": "application/json"}
    if use_anthropic_auth:
        headers["x-api-key"] = API_KEY
    else:
        headers["Authorization"] = f"Bearer {API_KEY}"
    if PROTOCOL == "messages":
        headers["anthropic-version"] = ANTHROPIC_VERSION
        if ANTHROPIC_BETA:
            headers["anthropic-beta"] = ANTHROPIC_BETA
            path = f"{path}?beta=true"

    request = urllib.request.Request(
        f"{API_URL}{path}",
        data=json.dumps(body).encode("utf-8"),
        headers=headers,
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
            if response.status != 200:
                fail(f"{path} returned HTTP {response.status}")
            return json.load(response)
    except urllib.error.HTTPError as error:
        fail(f"{path} returned HTTP {error.code}")
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
        fail(f"{path} transport/JSON failure: {type(error).__name__}")
    raise AssertionError("unreachable")


def verify_count_tokens() -> None:
    _, body = build_request(1)
    body.pop("max_tokens", None)
    payload = request_json(
        "/v1/messages/count_tokens", body, use_anthropic_auth=True
    )
    count = payload.get("input_tokens")
    if not isinstance(count, int) or count <= 0:
        fail("count_tokens did not return a positive input_tokens integer")
    print(f"count_tokens input={count} auth=x-api-key")


def run_turn(turn: int) -> tuple[int, int, int]:
    path, body = build_request(turn)
    payload = request_json(path, body)
    return usage_counts(payload)


def main() -> None:
    if not API_KEY:
        fail("API_KEY is required")
    if API_URL == "https://cloud-api.near.ai" and os.environ.get("ALLOW_PRODUCTION") != "1":
        fail("production is blocked; set ALLOW_PRODUCTION=1 only with explicit approval")

    print(f"Anthropic cache walk: protocol={PROTOCOL} api={API_URL} model={MODEL}")
    if PROTOCOL == "messages":
        verify_count_tokens()
    cache_reads: list[int] = []
    for turn in range(1, 5):
        prompt, cache_read, cache_write = run_turn(turn)
        cache_reads.append(cache_read)
        print(
            f"turn={turn} prompt={prompt} cache_read={cache_read} "
            f"cache_write={cache_write}"
        )
        if turn < 4:
            time.sleep(DELAY_SECONDS)

    if cache_reads[1] <= 0:
        fail("turn 2 did not read from the prompt cache")
    if not all(current > previous for previous, current in zip(cache_reads[1:], cache_reads[2:])):
        fail(f"cache reads did not advance after turn 2: {cache_reads}")
    print(f"PASS: cache reads advanced across the tool loop: {cache_reads}")


if __name__ == "__main__":
    main()
