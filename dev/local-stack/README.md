# Local dev stack — full tier-routing environment on one machine

Runs the real cloud-api against a **real inference engine on CPU** with the
GLM-5.2-style two-tier context topology (base fleet + long-context tier), so
routing changes can be exercised end to end without touching staging or any
GPU host:

```
curl ──► cloud-api :13000 (cargo run, mock auth, ephemeral signing keys)
             │                        ▲ Postgres :15432 (docker)
             ├─ base tier ─► tier_shim :18100 (ctx 1000) ──┐
             └─ long tier ─► tier_shim :18101 (ctx 8000) ──┴─► llama.cpp :18090
                                                (docker, CPU, Qwen2.5-0.5B GGUF)
```

The tier shims play the role of the per-tier SGLang fleets: they advertise the
canonical model id, serve **real completions** and **real token counts**
(llama.cpp's tokenizer) through the vLLM/SGLang-shaped `/v1/tokenize`, enforce
their tier's context window with the exact SGLang error phrasing (so
cloud-api's context-400 fall-through matcher fires as in prod), tag responses
(`"model": "<id>+base"` / `+long`) so you can SEE which tier served, and offer
a saturation drill (`touch SATURATE-long` → 503 "queue full").

## Quick start

```bash
cd dev/local-stack
make up          # postgres + llama.cpp (first run downloads ~400MB GGUF) + shims
make api         # cloud-api in the foreground (separate terminal)
make seed        # mock admin user, two-tier model, org + credits, API key
make demo        # the tier-routing demo table (8 checks)
make down        # stop everything
```

`make demo` exercises: small→base (long tier untouched), oversize→long,
boundary-sized→exact-tokenize decides (real counts beat the byte heuristic),
streaming→long over SSE, and saturated-long→retryable 429/5xx (never a
misleading context-length 400).

Manual poking:

```bash
KEY=$(cat .api-key)
curl -s localhost:13000/v1/chat/completions -H "Authorization: Bearer $KEY" \
  -d '{"model":"z-ai/glm-5.2-local","messages":[{"role":"user","content":"hi"}],"max_tokens":16}' \
  | jq .model     # → "z-ai/glm-5.2-local+base"
make logs         # shim access logs: tier decisions, tokenize hits, 400s/503s
```

## model-proxy control plane

`make proxy-demo` (needs a model-proxy checkout, default `../model-proxy`,
override with `MODEL_PROXY_DIR=…`) runs the REAL model-proxy binary with fast
discovery/health intervals and drives the GLM-5.2 long-context registration
flow against it: dual probes on one host (canonical id on `:18000`, synthetic
`-long` id on `:18001`) sharing one routed backend, the probe-cleanup
ownership guard (unregistering one probe must not drop the shared backend),
and the health-gated discovery stub + circuit breaker (non-2xx = unhealthy).

## What is intentionally NOT local

- **The TLS/SNI data path through model-proxy.** cloud-api's providers
  fail-closed on HTTPS to backends whose attestation didn't verify
  (fingerprint pinning) — that's the TEE trust model working. Locally the
  data path is plain HTTP direct to the tier shims (the provider's
  non-rotation fallback-client path, same code that serves IP-literal URLs);
  model-proxy is exercised on its control plane beside it.
- **The Chutes wire client** (ML-KEM E2EE + TDX quote verification against
  vetted measurements — not mockable at the HTTP level by design). The
  fallback CHAIN including a pinned attested provider is covered at the pool
  boundary in `crates/api/tests/e2e_all/glm52_tier_routing.rs`; the wire
  client runs in prod today for GLM-5.1/5.2.

## Engine notes

llama.cpp was chosen because it starts in seconds on CPU with a tiny GGUF and
exposes an OpenAI-compatible API plus a native `/tokenize`. The shims speak
OpenAI upstream, so any OpenAI-compatible engine (e.g. a vLLM CPU build) can
be swapped into `docker-compose.yml` without touching anything else.

Ports used: 13000 (api), 15432 (postgres), 18090 (engine), 18100/18101
(shims) — chosen not to clash with the e2e `test-postgres` on 5432.
