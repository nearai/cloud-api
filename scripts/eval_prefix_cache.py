#!/usr/bin/env python3
"""Evaluate prefix cache hit rate across inference backends.

Sends batches of requests with shared system prompts to measure how well
prefix caching works with the current routing. Run before and after
prefix-aware routing to compare.

Usage:
    # Against staging (default)
    python3 scripts/eval_prefix_cache.py

    # Against production
    API_URL=https://cloud-api.near.ai API_KEY=sk-... python3 scripts/eval_prefix_cache.py

    # Custom model
    MODEL=glm-4-0520 python3 scripts/eval_prefix_cache.py
"""

import os
import time
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from collections import defaultdict

try:
    import requests
except ImportError:
    print("pip install requests")
    sys.exit(1)

API_URL = os.environ.get("API_URL", "https://cloud-stg-api.near.ai")
API_KEY = os.environ.get("API_KEY", "")
MODEL = os.environ.get("MODEL", "qwen3.5-122b-a10b")
NUM_REQUESTS = int(os.environ.get("NUM_REQUESTS", "20"))
CONCURRENCY = int(os.environ.get("CONCURRENCY", "4"))
MAX_TOKENS = int(os.environ.get("MAX_TOKENS", "32"))

HEADERS = {
    "Authorization": f"Bearer {API_KEY}",
    "Content-Type": "application/json",
}

# A long system prompt that should be cached across requests
SYSTEM_PROMPT = """You are a helpful AI assistant specialized in software engineering.
You provide concise, accurate answers about programming, system design, and debugging.
When asked about code, you explain the reasoning behind your suggestions.
You follow best practices for security, performance, and maintainability.
You are familiar with Rust, Python, TypeScript, and infrastructure tooling.
When uncertain, you say so rather than guessing.
You format code examples with proper syntax highlighting.
You consider edge cases and error handling in your suggestions.
""" * 5  # Repeat to make it longer (~2000 chars) for better prefix cache testing

# Different user prompts — all share the same system prompt prefix
USER_PROMPTS = [
    "What is the difference between a mutex and a semaphore?",
    "Explain the CAP theorem in one paragraph.",
    "How does TCP congestion control work?",
    "What is the purpose of the Linux OOM killer?",
    "Describe the difference between stack and heap allocation.",
    "What is a consistent hash ring?",
    "How does TLS 1.3 differ from TLS 1.2?",
    "What is copy-on-write in operating systems?",
    "Explain the difference between threads and coroutines.",
    "What is the purpose of an L4 vs L7 load balancer?",
]


def send_completion(prompt_idx: int, request_num: int) -> dict:
    """Send a chat completion request and return timing + metadata."""
    user_prompt = USER_PROMPTS[prompt_idx % len(USER_PROMPTS)]
    body = {
        "model": MODEL,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_prompt},
        ],
        "max_tokens": MAX_TOKENS,
        "stream": False,
    }

    t0 = time.monotonic()
    try:
        resp = requests.post(
            f"{API_URL}/v1/chat/completions",
            headers=HEADERS,
            json=body,
            timeout=60,
        )
        elapsed = time.monotonic() - t0

        if resp.status_code != 200:
            return {
                "request_num": request_num,
                "prompt_idx": prompt_idx,
                "status": resp.status_code,
                "error": resp.text[:200],
                "elapsed": elapsed,
            }

        data = resp.json()
        usage = data.get("usage", {})
        prompt_details = usage.get("prompt_tokens_details") or {}
        chat_id = data.get("id", "")

        return {
            "request_num": request_num,
            "prompt_idx": prompt_idx,
            "chat_id": chat_id,
            "status": 200,
            "elapsed": elapsed,
            "prompt_tokens": usage.get("prompt_tokens", 0),
            "completion_tokens": usage.get("completion_tokens", 0),
            "cached_tokens": prompt_details.get("cached_tokens", 0),
            "ttft": elapsed,  # approximation for non-streaming
        }
    except Exception as e:
        return {
            "request_num": request_num,
            "prompt_idx": prompt_idx,
            "status": 0,
            "error": str(e),
            "elapsed": time.monotonic() - t0,
        }


def fetch_signature_backend(chat_id: str) -> str | None:
    """Fetch signature for a chat_id — the signing_address identifies the backend."""
    try:
        resp = requests.get(
            f"{API_URL}/v1/signature/{chat_id}?signing_algo=ed25519",
            headers=HEADERS,
            timeout=10,
        )
        if resp.status_code == 200:
            return resp.json().get("signing_address", "")
    except Exception:
        pass
    return None


def discover_backends() -> dict:
    """Discover backends by making parallel attestation calls."""
    backends = {}
    # Use the inference URL directly if accessible, otherwise use cloud-api attestation
    try:
        resp = requests.get(
            f"{API_URL}/v1/attestation/report?model={MODEL}&signing_algo=ed25519&include_tls_fingerprint=true",
            headers=HEADERS,
            timeout=15,
        )
        if resp.status_code == 200:
            data = resp.json()
            for att in data.get("model_attestations", []):
                addr = att.get("signing_address", "")
                fp = att.get("tls_cert_fingerprint", "")
                if addr:
                    backends[addr[:16]] = fp[:16] if fp else "no-fp"
    except Exception:
        pass
    return backends


def main():
    print(f"=== Prefix Cache Evaluation ===")
    print(f"API: {API_URL}")
    print(f"Model: {MODEL}")
    print(f"Requests: {NUM_REQUESTS} (concurrency={CONCURRENCY})")
    print(f"System prompt: {len(SYSTEM_PROMPT)} chars")
    print(f"Max tokens: {MAX_TOKENS}")
    print()

    # Phase 1: Discover backends
    print("Discovering backends...")
    backends = discover_backends()
    if backends:
        print(f"  Found {len(backends)} backend(s):")
        for addr, fp in backends.items():
            print(f"    signing_addr={addr}...  tls_fp={fp}...")
    else:
        print("  Could not discover backends via attestation")
    print()

    # Phase 2: Warmup — send a few requests to prime the prefix cache
    print("Warmup: sending 2 requests to prime prefix cache...")
    for i in range(2):
        result = send_completion(0, -1)
        status = result.get("status", 0)
        cached = result.get("cached_tokens", 0)
        print(f"  warmup-{i+1}: status={status} cached_tokens={cached} elapsed={result['elapsed']:.2f}s")
    print()

    # Phase 3: Measure — send requests with shared system prompt
    print(f"Sending {NUM_REQUESTS} requests (shared system prompt, varied user prompts)...")
    results = []
    with ThreadPoolExecutor(max_workers=CONCURRENCY) as executor:
        futures = {}
        for i in range(NUM_REQUESTS):
            prompt_idx = i % len(USER_PROMPTS)
            f = executor.submit(send_completion, prompt_idx, i)
            futures[f] = i

        for f in as_completed(futures):
            result = f.result()
            results.append(result)
            i = result["request_num"]
            cached = result.get("cached_tokens", 0)
            prompt = result.get("prompt_tokens", 0)
            status = result.get("status", 0)
            elapsed = result.get("elapsed", 0)
            pct = f"{cached/prompt*100:.0f}%" if prompt > 0 else "n/a"
            err = f" error={result.get('error', '')[:60]}" if status != 200 else ""
            print(f"  req-{i:02d}: status={status} prompt={prompt} cached={cached} ({pct}) elapsed={elapsed:.2f}s{err}")

    print()

    # Phase 4: Analyze results
    successful = [r for r in results if r.get("status") == 200]
    failed = [r for r in results if r.get("status") != 200]

    if not successful:
        print("All requests failed!")
        return

    total_prompt = sum(r.get("prompt_tokens", 0) for r in successful)
    total_cached = sum(r.get("cached_tokens", 0) for r in successful)
    avg_elapsed = sum(r["elapsed"] for r in successful) / len(successful)
    cache_rate = total_cached / total_prompt * 100 if total_prompt > 0 else 0

    print(f"=== Results ===")
    print(f"Successful: {len(successful)}/{NUM_REQUESTS}")
    print(f"Failed: {len(failed)}/{NUM_REQUESTS}")
    print(f"Total prompt tokens: {total_prompt}")
    print(f"Total cached tokens: {total_cached}")
    print(f"Cache hit rate: {cache_rate:.1f}%")
    print(f"Avg latency: {avg_elapsed:.2f}s")
    print()

    # Phase 5: Check backend distribution via signatures
    print("Checking backend distribution via signatures...")
    backend_counts = defaultdict(int)
    for r in successful:
        chat_id = r.get("chat_id", "")
        if not chat_id:
            continue
        addr = fetch_signature_backend(chat_id)
        if addr:
            key = addr[:16]
            backend_counts[key] += 1
            time.sleep(0.1)  # gentle rate limit

    if backend_counts:
        print(f"  Requests distributed across {len(backend_counts)} backend(s):")
        for addr, count in sorted(backend_counts.items(), key=lambda x: -x[1]):
            pct = count / sum(backend_counts.values()) * 100
            print(f"    {addr}...: {count} requests ({pct:.0f}%)")
    else:
        print("  Could not determine backend distribution")

    print()
    print(f"=== Summary ===")
    print(f"Cache hit rate: {cache_rate:.1f}% (higher is better)")
    print(f"Backend spread: {len(backend_counts)} backends (lower = better for cache)")
    if cache_rate < 10:
        print("→ Low cache rate — prefix routing should help significantly")
    elif cache_rate < 50:
        print("→ Moderate cache rate — prefix routing can improve this")
    else:
        print("→ Good cache rate — prefix routing may have diminishing returns")


if __name__ == "__main__":
    main()
