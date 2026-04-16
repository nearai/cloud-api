#!/usr/bin/env python3
"""Integration test for prefix-aware routing and signature fetch.

Verifies:
1. Requests with the same system prompt hit the same backend (same signing_address)
2. Requests with different system prompts may hit different backends
3. Signature fetch succeeds for all completions (proves bucket client pinning works)
4. Streaming and non-streaming paths both work

Usage:
    API_KEY=sk-... python3 scripts/test_prefix_routing.py
    API_KEY=sk-... API_URL=https://cloud-api.near.ai MODEL=zai-org/GLM-5-FP8 python3 scripts/test_prefix_routing.py
"""

import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

try:
    import requests
except ImportError:
    print("pip install requests")
    sys.exit(1)

API_URL = os.environ.get("API_URL", "https://cloud-stg-api.near.ai")
API_KEY = os.environ.get("API_KEY", "")
MODEL = os.environ.get("MODEL", "qwen3.5-122b-a10b")

if not API_KEY:
    print("ERROR: API_KEY env var required")
    sys.exit(1)

HEADERS = {
    "Authorization": f"Bearer {API_KEY}",
    "Content-Type": "application/json",
}

SYSTEM_A = "You are an expert mathematician. Always show your work step by step." * 10
SYSTEM_B = "You are a creative fiction writer. Use vivid imagery and metaphors." * 10


def completion_and_signature(system_prompt, user_msg, stream=False):
    """Send a completion, then fetch its signature. Returns (chat_id, signing_address, stream)."""
    body = {
        "model": MODEL,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_msg},
        ],
        "max_tokens": 16,
        "stream": stream,
    }

    t0 = time.monotonic()

    if stream:
        resp = requests.post(
            f"{API_URL}/v1/chat/completions",
            headers=HEADERS,
            json=body,
            stream=True,
            timeout=60,
        )
        if resp.status_code != 200:
            return {"error": f"HTTP {resp.status_code}: {resp.text[:200]}"}

        chat_id = None
        for line in resp.iter_lines():
            line = line.decode("utf-8", errors="replace")
            if line.startswith("data: ") and line != "data: [DONE]":
                import json
                try:
                    chunk = json.loads(line[6:])
                    if "id" in chunk:
                        chat_id = chunk["id"]
                except Exception:
                    pass
        resp.close()
    else:
        resp = requests.post(
            f"{API_URL}/v1/chat/completions",
            headers=HEADERS,
            json=body,
            timeout=60,
        )
        if resp.status_code != 200:
            return {"error": f"HTTP {resp.status_code}: {resp.text[:200]}"}
        chat_id = resp.json().get("id")

    elapsed = time.monotonic() - t0

    if not chat_id:
        return {"error": "no chat_id in response"}

    # Fetch signature — this is the critical test: it must hit the same backend
    time.sleep(0.5)  # Brief delay for async signature storage
    sig_resp = requests.get(
        f"{API_URL}/v1/signature/{chat_id}?signing_algo=ed25519",
        headers=HEADERS,
        timeout=15,
    )

    if sig_resp.status_code == 200:
        sig = sig_resp.json()
        return {
            "chat_id": chat_id,
            "signing_address": sig.get("signing_address", ""),
            "signature_ok": True,
            "stream": stream,
            "elapsed": elapsed,
        }
    else:
        return {
            "chat_id": chat_id,
            "signing_address": "",
            "signature_ok": False,
            "signature_status": sig_resp.status_code,
            "stream": stream,
            "elapsed": elapsed,
        }


def run_test():
    passed = 0
    failed = 0

    def check(name, condition, detail=""):
        nonlocal passed, failed
        if condition:
            passed += 1
            print(f"  PASS: {name}")
        else:
            failed += 1
            print(f"  FAIL: {name} — {detail}")

    print(f"=== Prefix Routing Integration Test ===")
    print(f"API: {API_URL}")
    print(f"Model: {MODEL}")
    print()

    # --- Test 1: Same system prompt → same backend ---
    print("Test 1: Same system prompt → same backend (non-streaming)")
    results_a = []
    for i in range(4):
        r = completion_and_signature(SYSTEM_A, f"What is {i+1}+{i+1}?", stream=False)
        if "error" in r:
            print(f"  ERROR: {r['error']}")
            continue
        results_a.append(r)
        addr = r["signing_address"][:16]
        print(f"  req-{i}: chat_id={r['chat_id'][:12]}... addr={addr}... sig_ok={r['signature_ok']}")

    if len(results_a) >= 2:
        addrs_a = set(r["signing_address"] for r in results_a)
        check(
            "all requests hit same backend",
            len(addrs_a) == 1,
            f"got {len(addrs_a)} distinct backends: {[a[:16] for a in addrs_a]}",
        )
        check(
            "all signatures fetched successfully",
            all(r["signature_ok"] for r in results_a),
            f"failures: {[r['chat_id'][:12] for r in results_a if not r['signature_ok']]}",
        )
    else:
        print("  SKIP: not enough successful requests")
    print()

    # --- Test 2: Same system prompt → same backend (streaming) ---
    print("Test 2: Same system prompt → same backend (streaming)")
    results_a_stream = []
    for i in range(4):
        r = completion_and_signature(SYSTEM_A, f"What is {i+10}*{i+2}?", stream=True)
        if "error" in r:
            print(f"  ERROR: {r['error']}")
            continue
        results_a_stream.append(r)
        addr = r["signing_address"][:16]
        print(f"  req-{i}: chat_id={r['chat_id'][:12]}... addr={addr}... sig_ok={r['signature_ok']}")

    if len(results_a_stream) >= 2:
        addrs_as = set(r["signing_address"] for r in results_a_stream)
        check(
            "streaming: all requests hit same backend",
            len(addrs_as) == 1,
            f"got {len(addrs_as)} distinct backends",
        )
        check(
            "streaming: all signatures fetched",
            all(r["signature_ok"] for r in results_a_stream),
        )
    print()

    # --- Test 3: Different system prompt → may hit different backend ---
    print("Test 3: Different system prompt (may hit different backend)")
    results_b = []
    for i in range(3):
        r = completion_and_signature(SYSTEM_B, f"Write about topic {i}", stream=False)
        if "error" in r:
            print(f"  ERROR: {r['error']}")
            continue
        results_b.append(r)
        addr = r["signing_address"][:16]
        print(f"  req-{i}: addr={addr}... sig_ok={r['signature_ok']}")

    if results_b:
        check(
            "all signatures fetched for prompt B",
            all(r["signature_ok"] for r in results_b),
        )
    if results_a and results_b:
        addrs_all_a = set(r["signing_address"] for r in results_a)
        addrs_all_b = set(r["signing_address"] for r in results_b)
        # Note: with model-proxy L4 and limited backends, they may still hit the same one
        if addrs_all_a != addrs_all_b:
            print("  INFO: different system prompts hit different backends (good)")
        else:
            print("  INFO: same backend for both prompts (possible with few backends)")
    print()

    # --- Test 4: Concurrent requests with same prefix ---
    print("Test 4: Concurrent requests with same system prompt")
    with ThreadPoolExecutor(max_workers=4) as executor:
        futures = {
            executor.submit(
                completion_and_signature, SYSTEM_A, f"Concurrent question {i}", False
            ): i
            for i in range(4)
        }
        results_conc = []
        for f in as_completed(futures):
            r = f.result()
            if "error" not in r:
                results_conc.append(r)
                addr = r["signing_address"][:16]
                i = futures[f]
                print(f"  req-{i}: addr={addr}... sig_ok={r['signature_ok']}")
            else:
                print(f"  req-{futures[f]}: ERROR {r['error'][:80]}")

    if len(results_conc) >= 2:
        addrs_conc = set(r["signing_address"] for r in results_conc)
        check(
            "concurrent: all hit same backend",
            len(addrs_conc) == 1,
            f"got {len(addrs_conc)} distinct backends",
        )
        check(
            "concurrent: all signatures fetched",
            all(r["signature_ok"] for r in results_conc),
        )
    print()

    # --- Summary ---
    total = passed + failed
    print(f"=== Results: {passed}/{total} passed, {failed}/{total} failed ===")
    if failed > 0:
        sys.exit(1)


if __name__ == "__main__":
    run_test()
