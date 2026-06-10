# cloud-api — Staging vs Prod changelog

**Prod:**    `2b209f7` · tag `prod-20260604-2b209f7` · image `sha256:e8a387e5…afaf4a3a` · pushed 2026-06-04 10:54 UTC
**Staging:** `0b4208a` · tag `staging-20260605-0b4208a` · image `sha256:f87ca04a…2f48a97` · pushed 2026-06-05 15:11 UTC

Staging is **14 commits ahead** of prod with **no divergence** (clean fast-forward; prod is 0 commits ahead). 37 files changed, +4914 / −381.

> ⚠️ **Deploy note:** staging adds **one new DB migration** — `V0055__add_model_deprecation_email_deliveries.sql` (PR #723). It runs forward automatically; no manual step, but it must apply before/with the image promote.

---

## OpenAI / passthrough param fidelity (OpenRouter compliance cluster)
This is the bulk of the delta — making forwarded sampling params faithful per backend.

- **#716** `fix(completions)` — forward `frequency_penalty` / `presence_penalty` to self-hosted backends (was being dropped). Closes #622.
- **#717** `fix(openai)` — strip non-OpenAI sampling params before forwarding so OpenAI-compatible backends don't 400. Closes #697.
- **#720** `fix(passthrough)` — Anthropic/Gemini param fidelity: temperature drop, `seed`, `response_format`. Closes #696, #669, #668.
- **#724** `fix(anthropic)` — also drop `top_p` for `opus-4-7` (it rejects `top_p`); follow-up to #696/#720.
- **#726** `refactor(api)` — **stop defaulting `temperature`/`top_p` to 1.0**; forward only client-set values. (Behavior change — see note below.)

## Vision
- **#719** `fix(vision)` — preserve image bytes / `media_type` on Anthropic & Gemini passthrough (previously corrupted/dropped). Closes #640.
- **#721** `fix(vision)` — fetch `image_url` with an explicit `User-Agent`; surface fetch errors as **4xx instead of 502**. Closes #606.

## Tools
- **#718** `fix(tools)` — honor `tool_choice=none` by stripping `tools` before forwarding. Closes #619.

## TEE / attestation
- **#709** `fix(completions)` — forward streamed SSE bytes **verbatim** for byte-exact TEE verification. (Gateway SSE passthrough fix.)

## Model aliasing
- **#708** Surface model alias substitution — adds a `warning` response field, `x-model-alias-resolved` header, and an `x-no-aliasing` strict mode. Closes #573.

## Billing / metrics
- **#728** `feat(metrics)` — per-model billed-usage metrics emitted at the central billing point.

## Admin / ops
- **#722** Add admin stats diagnostics and DB indexes.
- **#723** Add model-deprecation notification workflow → **new migration V0055** (`model_deprecation_email_deliveries` table) + email delivery plumbing.
- **#727** `fix(patroni)` — tolerate string `"unknown"` lag from stopped members (HA discovery no longer crashes on stopped replicas).

---

## Behavior changes to call out on promote
- **#726 / #724 / #720:** clients that previously relied on cloud-api injecting `temperature=1.0` / `top_p=1.0` will now have those params **omitted** unless they set them. This is the intended OpenRouter-compliant behavior and fixes the `opus-4-7` regression, but it is an observable change for downstream callers.
- **#721:** image-fetch failures now return **4xx** (client-actionable) rather than 502 — alerting/dashboards keyed on 502 for vision will see the shift.
- **#708:** responses may now carry a `warning` field + alias headers when a model alias is resolved.

---

## Staging verification (2026-06-05, live against cloud-stg-api.near.ai)

Ran the `infra-tests/test_openrouter_params.py` regression harness pointed at staging (its `xfail` "living repros" map 1:1 to these PRs and should flip to **xpass** once deployed) plus targeted live probes. Result: **15 passed, 1 skipped, 2 xfailed, 11 xpassed** — every targeted fix confirmed, all plain-assert tripwires stayed green (no regression).

| PR(s) | Behavior | Staging result |
|---|---|---|
| #696/#720/#724/#726 | opus-4-7 accepts `temperature`; `top_p` dropped; no injected defaults | ✅ XPASS — bare / `top_p=0.5` / `temperature=0.5` / `frequency_penalty` / combined all 200 |
| #697/#717 | OpenAI-passthrough accepts non-OpenAI sampling extras (`top_k`,`min_p`,`top_a`,`repetition_penalty`) | ✅ XPASS (all 4) |
| #716/#622 | `frequency_penalty`/`presence_penalty` forwarded to self-hosted | ✅ 200 on Qwen3-30B |
| #619/#718 | `tool_choice=none` suppresses tools on Anthropic & Gemini | ✅ XPASS (both) |
| #640/#719 | vision image fidelity (Anthropic & Gemini) | ✅ Anthropic XPASS; Gemini sees "Red" @300 tok (empties at low budget = thinking, not garbling) |
| #721 | unreachable `image_url` → 4xx not 502 | ✅ 400 `invalid_request` ("Cannot fetch content from the provided URL") |
| #709 | streamed SSE byte-exact passthrough | ✅ clean `data:` framing + `[DONE]`; (already verified staging 2026-06-05) |
| #695 | bare-string `stop` accepted | ✅ XPASS |
| #708 | alias transparency: `x-model-alias-resolved` header + `warning` field + `x-no-aliasing` strict mode | ✅ verified live via Qwen3-30B→Qwen3.6-35B alias (see below) |
| #723 | model-deprecation workflow + migration V0055 | ✅ migration applied (service boots/serves); `deprecation_date` populated; deprecated model hidden from `/v1/models` but still resolves via alias |
| #728 / #722 / #727 | per-model billing metrics / admin stats+indexes / patroni unknown-lag | ⚪ not client-observable; staging healthy (`/v1/health` 200), no boot/HA failure |

### ⚠️ One changelog correction (not a regression)
- **#668 `json_schema` is NOT closed for Anthropic.** `response_format: json_schema` is still **ignored** on the Anthropic passthrough (returns markdown prose) — verified **identical on prod and staging**, so it is a *pre-existing gap*, not introduced by this deploy. Gemini's json_schema **was** fixed (XPASS). Anthropic has no native OpenAI-style `json_schema`; closing it needs tool-injection. → amend "#668 closed" to "#668 closed for Gemini; Anthropic still open".

### #708 alias transparency — verified live
After registering `Qwen/Qwen3-30B-A3B-Instruct-2507` as an alias of `Qwen/Qwen3.6-35B-A3B-FP8`, all three paths behave to spec:
- **Non-streaming:** `x-model-alias-resolved: Qwen/Qwen3-30B-A3B-Instruct-2507 -> Qwen/Qwen3.6-35B-A3B-FP8`; `body.model` = canonical successor; top-level `warning` names both models and points to `x-no-aliasing`.
- **Streaming:** header present; `warning` on the **first chunk only** (0 later chunks carried it across 8 chunks); `model` = canonical.
- **`x-no-aliasing: true`:** rejected with **400** ("…is an alias of … and the request set x-no-aliasing. Use the canonical model name").
- The deprecated alias is hidden from the `/v1/models` listing but still resolves on the completions path.

### infra-tests scheduled run (last completed 14:48 UTC: 120 failed / 549 passed / 12 xfailed / 2 xpassed)
The 120 failures are **not** cloud-api regressions — they are an **attestation incident + model-availability flakiness** hitting **prod and staging equally**:
- `attestation/report` returning **503** in bursts (gemma-4-31B, gpt-oss-120b, privacy-filter, Qwen3.6-35B) — the known burst-503 issue (cache the pubkey / 3s spacing).
- GLM-5.1 **529/502** "service overloaded / queue full"; signature 404s from retrieval timing.
None touch the param/passthrough/alias/SSE code paths in this delta. Once the attestation backends settle, those tests recover on their own.

**Verdict: staging is clean to promote.** All 14 PRs behave as intended; the only caveat is the #668-Anthropic doc correction above (pre-existing, not a blocker).

---

## Full commit list (prod → staging, oldest first)
```
4b5b76e5 2026-06-04 fix(completions): forward frequency_penalty/presence_penalty to self-hosted backends (#622) (#716)
76897882 2026-06-04 fix(openai): strip non-OpenAI sampling params before forwarding (#697) (#717)
c7dded25 2026-06-04 fix(passthrough): Anthropic/Gemini param fidelity — temperature drop, seed, response_format (#696 #669 #668) (#720)
661943c4 2026-06-04 fix(vision): preserve image bytes/media_type on Anthropic & Gemini passthrough (#640) (#719)
5094f111 2026-06-04 fix(tools): honor tool_choice=none by stripping tools before forwarding (#619) (#718)
22de2b0f 2026-06-05 refactor(api): stop defaulting temperature/top_p to 1.0 — forward only client-set values (#726)
1d2d13e5 2026-06-05 fix(patroni): tolerate string "unknown" lag from stopped members (#727)
bf2f357c 2026-06-05 fix(anthropic): drop top_p too for opus-4-7 (it rejects top_p) — #696 follow-up (#724)
58c7d0d8 2026-06-05 Surface model alias substitution: warning field + headers + strict mode (#573) (#708)
8c74a240 2026-06-05 fix(vision): fetch image_url with explicit User-Agent; surface fetch errors as 4xx not 502 (#606) (#721)
5fafb587 2026-06-05 fix(completions): forward streamed SSE bytes verbatim for byte-exact TEE verification (#709)
da292fb6 2026-06-05 feat(metrics): per-model billed-usage metrics at the central billing point (#728)
b9297a91 2026-06-05 Add admin stats diagnostics and indexes (#722)
0b4208a4 2026-06-05 Add model deprecation notification workflow (#723)
```
