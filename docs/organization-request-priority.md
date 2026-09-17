# Organization request priority

Platform admins control the scheduling priority of Cloud API requests to NEAR AI
inference backends. Higher values run ahead of lower values in a priority-enabled
SGLang waiting queue. The default is `0`; the supported range is `-1000` through
`1000`. A value of `-2` runs below a gateway lane configured at `-1`.

## Admin API

- `GET /v1/admin/organizations/{org_id}/priority`
- `PATCH /v1/admin/organizations/{org_id}/priority` with `{"priority": -2}`

Both return `{"organization_id": "<org UUID>", "priority": -2}`. PATCH requires
an integer; setting `0` restores the default. Missing and inactive organizations
return 404. Out-of-range values return 400.

Platform-admin sessions and write-enabled admin tokens can update the setting.
Read-only admin tokens can GET/HEAD it. Organization owners, organization admins,
and inference API keys cannot read or update it. The setting is stored separately
from organization JSON settings and omitted from customer-facing responses.

## Request behavior

Authentication loads the priority with the organization, without another database
lookup or priority cache. The next authenticated request sees a committed update;
already-running requests, their tool iterations, and background title generation
retain the priority captured when the request began.

Chat completions, legacy text completions, and Responses use the same internal
metadata, including streaming and non-streaming calls and provider retries. The
NEAR AI provider sends `X-NearAI-Priority` using that metadata. Customer headers
and body fields cannot override it. Internal priority metadata is never serialized
into provider JSON and is not sent in external-provider headers. OpenAI
`service_tier` remains independent.

Enforcement requires a trusted Cloud API backend token and an inference proxy
that supports the priority header, plus enabled engine priority scheduling.
Direct customer API-key calls to inference-proxy retain their existing priority.
This migration does not assign priorities to individual organizations or change
gateway or engine configuration.

## Rollout validation

Apply the additive migration before the new server handles requests (the normal
startup migration path does this). Old servers remain compatible with the added
column; use a schema-preserving application rollback if needed.

Before production rollout, validate the new Cloud API build against staging CVM
proxies and priority-enabled SGLang: read/write a synthetic org's priority, verify
its effective engine bucket for JSON and completed SSE calls, repeat after a
priority update, and check every serving replica. Observe numeric priority counters
and aggregate engine metrics only. Reset the test org to `0` after validation.
