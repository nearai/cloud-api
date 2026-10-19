# External provider routing policy

OpenAI-compatible models can configure extra request fields in `provider_config`.
`extra_request_body` supplies defaults that callers can override.
`enforced_request_body` applies mandatory fields after those defaults and caller
parameters. Objects merge recursively; configured leaves and arrays replace
caller values. A caller's null, scalar, or array cannot replace a configured object.
Models without this optional field retain existing behavior.

For example, this OpenRouter configuration restricts inference to a selected
provider's ZDR endpoints, without fallback:

```json
{
  "backend": "openai_compatible",
  "base_url": "https://openrouter.ai/api/v1",
  "enforced_request_body": {
    "provider": {
      "only": ["fireworks"],
      "allow_fallbacks": false,
      "zdr": true,
      "data_collection": "deny"
    }
  }
}
```

The initial policy surface supports only the extra JSON `provider` object.
The admin write path and provider loader reject unknown external-config fields,
policies on other backends, empty or non-object provider policies, and other
enforced top-level keys (including typed or backend-normalized fields such as
`model`, `messages`, `reasoning_effort`, and `top_k`).
Audio transcription and image editing fail before dispatch when a mandatory
policy is configured, because multipart transport cannot carry these JSON fields.
Credentials belong in the provider's secret configuration, never in either map.

Caller fields outside the mandatory configuration retain their existing
semantics. For example, `provider.ignore` can exclude all allowed providers and
cause a request to fail; it cannot widen the permitted set or disable ZDR.

Deploy support to every serving replica before enabling a model that requires
this policy: older binaries ignore unknown configuration fields. Verify both
streaming and non-streaming requests with conflicting caller preferences. Apply
the policy to every provider configuration serving the model.

OpenRouter's [`zdr` filter](https://openrouter.ai/docs/guides/features/zdr)
restricts inference routing to eligible endpoints and fails if none are available.
It does not describe Cloud API conversation storage or separate plugin/tool
retention policies.

## Provider registry (issue #1273, first rollout stage)

Providers can be configured independently of catalog models. This stage manages
non-attested external adapters (`openai_compatible`, `anthropic`, `gemini`, and
`typesafe`). Existing NEAR/Chutes/Tinfoil registration and trust policy are
unchanged. Mixed-trust request policy, procurement prices, and new retry policy
remain later stages of #1273.

### Configure a provider

`POST /v1/admin/providers` creates a provider. `GET /v1/admin/providers` lists
providers, including archives. `PUT /v1/admin/providers/{id}` replaces its
configuration, including its archive state. There is no provider DELETE API.

```json
{
  "name": "openrouter-production",
  "config": {
    "backend": "openai_compatible",
    "base_url": "https://openrouter.ai/api/v1"
  },
  "credential_env": "OPENROUTER_API_KEY",
  "models_path": "models",
  "archived": false
}
```

Deploy the referenced environment variable to every serving replica first.
`credential_env: null` explicitly means unauthenticated access, not fallback to a
shared adapter key. References must be uppercase environment names ending in
`_API_KEY`. Registry configuration rejects inline `api_key` and `model_name`;
credentials and upstream model selection are independent of model metadata.
Existing mandatory `enforced_request_body` policies work in provider config.

`models_path` is relative to the configured API root. Set it to `models` for
OpenAI/OpenRouter/DeepSeek and Anthropic roots ending in `/v1` (or the equivalent
provider API root). Set it to null for manual inventory, including TypeSafe when
no supported discovery endpoint is available. This does not invent a model-list
API for protocols that do not have one. Discovery uses the adapter's auth scheme,
disallows redirects, and follows bounded cursors rather than arbitrary next URLs.

Saving a discovery-enabled provider attempts an initial sync. Sync failures do
not undo valid configuration: the returned `sync_error` reports failure and the
previous inventory is retained. `POST /v1/admin/providers/{id}/sync` explicitly
refreshes inventory. Background discovery runs at startup and every ten minutes;
DB advisory locks coordinate replicas and revision checks discard stale results.
`last_synced_at` records the last complete successful inventory.

### Approve models

`GET /v1/admin/providers/{id}/models` lists inventory, approval state, archive
state, availability, metadata, and effective reasoning efforts. Discovery never
creates customer-facing catalog models, sets prices, or approves routes.

Create/review a catalog model and its prices through the existing
`PATCH /v1/admin/models` API. For this stage it must have `providerType: external`,
`verifiable: false`, and `attestationSupported: false`. Then whitelist a model:

```http
PUT /v1/admin/providers/{id}/models
```

```json
{
  "upstream_model": "vendor/upstream-model",
  "model_name": "our/canonical-model",
  "approved": true,
  "archived": false,
  "reasoning_efforts": ["low", "medium", "high"]
}
```

The same operation adds a manual entry when discovery is disabled or the model
is absent from inventory. Approval does not activate or unarchive the catalog
model. Multiple providers can map to the same canonical model. Updating one
inventory entry replaces its mapping; the old canonical model remains
registry-managed, so moving or withdrawing its last route cannot resurrect its
legacy provider configuration.

`reasoning_efforts: null` removes the admin override and uses discovered metadata
(or provider-level configuration if discovery is unknown). An empty array means
explicitly unsupported. Unknown capabilities permit upstream validation but are
not advertised as supported. Refresh preserves admin overrides, approvals, and
archives. Missing discovered models become unavailable, not deleted. Manual
entries remain authoritative even when omitted by discovery.

### Archive and restore

```http
PUT /v1/admin/models/{url-encoded-model-name}/archive
```

```json
{"archived": true, "change_reason": "Temporarily withdrawn"}
```

Send `archived: false` to unarchive. Archiving preserves model IDs, aliases,
pricing, and history. Restoring recovers the pre-archive activation state; it does
not activate a previously disabled draft. Repeated operations are idempotent.
`GET /v1/admin/models?include_inactive=true` includes an `archived` flag.
Legacy DELETE-model requests also archive; they do not remove data.

A database trigger prevents ordinary updates, discovery, or seed upserts from
reactivating archived rows. Explicit unarchive is required. Provider archive uses
`archived` on its full PUT body. Route archive uses `archived` on its approval
body. Unarchiving a provider does not approve denied routes or restore archived
catalog models. Each registry mutation is recorded in `provider_registry_history`;
catalog lifecycle changes continue to use `model_history`.

Changes refresh the handling replica before a successful response. Other serving
replicas reconcile external routes every 30 seconds, independently of inventory
sync and NEAR fleet discovery. This bound assumes a reachable database and a
successful refresh; a database outage retains the last usable snapshot. Existing
model-resolution cache TTLs still apply. Unchanged external configurations reuse
their provider instances and connections. Requests already in flight can finish.

### Reasoning effort

Chat Completions preserves explicit `reasoning_effort`, including requests with
tools. Responses translation carries `reasoning.effort` into every tool-loop
completion. OpenRouter receives nested `reasoning.effort`; Anthropic receives
`output_config.effort` without discarding native thinking/output options. Both
raw-sidecar and synthesized Anthropic requests use the same translation.
Conflicting selectors or unsupported configured values fail explicitly.

OpenAI combinations that require native Responses transport still need the
Responses API. Chat requests are not silently rewritten to disable reasoning to
make an unsupported combination succeed. Unknown capabilities are passed through
for upstream validation; supported reasoning levels vary by model/provider.
TypeSafe decision requests retain their own protocol rather than pretending to
support chat reasoning.

### Migration and validation

V0095 is additive: registry tables, archive markers/history, and a persistent
registry-ownership marker on models. It does not copy credentials or automatically
migrate existing per-model configuration. Unmapped models keep their old loader;
operators adopt registry management by explicitly approving mappings. Do not roll
back to a legacy loader after adopting registry routes without reconciling those
models first, since old code does not understand registry ownership.

The `provider_registry_` E2E tests exercise admin handlers, real PostgreSQL and
route registration. Separate opt-in tests exercise public OpenRouter discovery
and credentialed provider discovery; missing credentials are not a passing test.
No upstream inference results are fabricated by these management tests.
