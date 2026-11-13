# Organization request priority

Platform admins control the scheduling priority of Cloud API requests to NEAR AI
inference backends. Higher values run ahead of lower values in a priority-enabled
SGLang waiting queue. The default is `0`; the supported range is `-1000` through
`1000`. A value of `-2` runs below a gateway lane configured at `-1`.

## Setting the priority

The priority is set through the admin API (`GET`/`PATCH /v1/admin/organizations/{org_id}/priority`,
range `-1000` to `1000`, `0` restores the default). Cloud API only reads it. The value is stored
separately from organization JSON settings and omitted from customer-facing responses.

## Request behavior

Authentication loads the priority with the organization, without another database
lookup or priority cache. The next authenticated request sees a committed update;
already-running requests, their tool iterations, and background title generation
retain the priority captured when the request began.

Chat completions, legacy text completions, and the typed Responses adapter
for non-OpenAI models use the same internal metadata, including streaming and
non-streaming calls, provider retries, and every Cloud-managed search round.
Native Responses for canonical `openai/` models uses a separate provider-native
path and does not accept client priority fields. Its `web_search` executes
upstream within that native request and creates no Cloud tool-loop rounds.
The NEAR AI provider sends `X-NearAI-Priority` using that metadata. Customer headers
and body fields cannot override it. Internal priority metadata is never serialized
into provider JSON and is not sent in external-provider headers. OpenAI
`service_tier` remains independent.

Enforcement requires a trusted Cloud API backend token and an inference proxy
that supports the priority header, plus enabled engine priority scheduling.
Direct customer API-key calls to inference-proxy retain their existing priority.
This migration does not assign priorities to individual organizations or change
gateway or engine configuration.

## Tests

`crates/api/tests/e2e_all/request_priority.rs` seeds an organization's priority and checks that
chat, text and Responses requests, JSON and streaming, carry it to the provider, that client-sent
priority values are ignored, and that concurrent requests from different organizations keep their
own priority.

## Rollout validation

Apply the additive schema change before the new server handles requests. cloud-platform
owns it, and the deploy's `cloud-platform migrate` step applies it; cloud-api does not migrate
at startup. Old servers remain compatible with the added
column; use a schema-preserving application rollback if needed.

Before production rollout, validate the new Cloud API build against staging CVM
proxies and priority-enabled SGLang: read/write a synthetic org's priority, verify
its effective engine bucket for JSON and completed SSE calls, repeat after a
priority update, and check every serving replica. Observe numeric priority counters
and aggregate engine metrics only. Reset the test org to `0` after validation.
