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

## Provider registry (issue #1273, staged implementation)

Providers can be configured independently of catalog models. This stage manages
non-attested external adapters (`openai_compatible`, `anthropic`, `gemini`, and
`typesafe`). Existing NEAR/Chutes/Tinfoil registration and trust policy are
retained. The registry also stores explicit routing policy, independent routes,
procurement cost versions, and attempt records. This is not completion evidence
for all of #1273; the outstanding acceptance gates are listed below.

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

For local management tests, use the debug-only `DEV=true` setting when dstack
is unavailable. Server startup also requires `BRAVE_SEARCH_PRO_API_KEY`; an
unused placeholder is sufficient for these tests, which do not invoke search:

```sh
DEV=true BRAVE_SEARCH_PRO_API_KEY=unused-management-test-placeholder \
DATABASE_HOST=localhost DATABASE_USERNAME=postgres DATABASE_PASSWORD=postgres \
TEST_DATABASE_NAME=cloud_api_provider_registry_e2e \
cargo test -p api --test e2e_all provider_registry_ -- --test-threads=1
```

To run public discovery, replace the test filter with
`provider_registry_public_openrouter_discovery_preserves_approval` and add
`--ignored` after `--`. This verifies discovery and retained admin decisions,
not upstream inference or vendor streaming compatibility.

### Accounts, manual inventory, and independent routes

`GET/PUT /v1/admin/provider-accounts` manages stable vendor/account identities.
The PUT body is `{ "account": { "id", "name", "vendor", "account_reference", "archived" }, "change_reason" }`.
Deployments reference `account_id`; account and deployment archives both remove
routes from eligibility. Deployment `quota_group` groups shared upstream quotas;
otherwise the credential reference or deployment UUID supplies the group.

`PUT /v1/admin/providers/{id}/inventory` accepts `upstream_model`, `available`,
`archived`, nullable `reasoning_efforts`, `provenance`, and `change_reason`.
It creates inventory without creating or approving a catalog route. Manual
inventory survives missing discovery results. TypeSafe requires this manual path.

`GET/PUT /v1/admin/models/{model_name}/routes` manages independent routes. PUT
accepts `{ "route": { "id", "model_id", "inventory_id", "approved", "enabled",
"archived", "reasoning_efforts", "context_limit", "output_limit", "role" },
"change_reason" }`. Identity fields are immutable; `role` is `primary` or
`fallback`. Multiple catalog models may use the same inventory entry. The legacy
mapping API only revokes mappings it still owns when remapping inventory.

### Routing policy and cost controls

`GET/PUT /v1/admin/models/{model_name}/routing-policy` reads or updates policy.
PUT wraps `policy` and a required `change_reason`. Policy includes:

- `default_tee_requirement`: `required` or `any`. Migration preserves existing
  TEE catalog defaults; request `tee` is validated before dispatch.
- `mixed_routing_enabled`: defaults false. Deploy compatible binaries on every
  replica before enabling mixed routes. A key pin or encrypted chat request
  strengthens the request to TEE even when it requests `any`.
- `max_attempts` (1–16) and `deadline_ms` (100–600000).
- Optional `max_attempt_cost_microusd` and `max_total_cost_microusd`.
- `allow_negative_margin`: explicit permission for retries whose planned
  cumulative procurement spend exceeds the accepted customer-price estimate.

`GET/POST /v1/admin/provider-routes/{id}/cost-profiles` manages versioned costs.
POST wraps `rates` and `change_reason`. Rates include `currency`, `billing_unit`
(`million_tokens`), `input_microunits`, `output_microunits`, nullable
`cache_read_microunits`/`cache_write_microunits`, and `provenance`. Integers represent
millionths of currency per million tokens. Updating creates a new version and
closes the prior interval; it does not change catalog customer prices.

Only eligible routes are cost-ranked. The shared retry path reserves each
attempt's estimate, including failed attempts. A configured cost ceiling rejects
unknown costs, unsupported currencies, or missing token bounds before dispatch.
Responses tool iterations retain an internal request-owned customer-price and
procurement-rate snapshot, deadline, and aggregate attempt/spend reservation.
Follow-ups cannot reset reservations or raise the initial customer-cost estimate.
Caller-supplied correlation IDs do not share budget state between requests.
These are planning limits, not guarantees about vendor invoices or tokenizer
estimation. Native Responses and Anthropic transports use the same pre-commit
retry bounds and shared health. Final HTTP status, headers, and body bytes are
preserved. Successful native bodies record EOF, failure, timeout, or cancellation
without replaying committed output. Bounded native usage observers retain
reported input, output, and cache usage for procurement estimates; malformed or
oversized usage remains unknown. Non-token billing units remain incomplete.

Rate limits and transient failures update shared PostgreSQL cooldowns; bounded
`Retry-After` is honored. Credential failures quarantine the quota group. Expired
cooldowns admit a single half-open probe across replicas. Registry snapshots
refresh every 30 seconds and registered external routes fail closed after 60
seconds without a successful refresh. Accepted streams are never retried by the
stream observer; cancellation leaves cost unknown unless explicitly reconciled.

### Attempt audit and reconciliation

`GET /v1/admin/provider-attempts/{request_id}` returns up to 1000 attempts linked
to the accepted request UUID. Entries identify route/deployment/cost revisions,
policy requirements, outcome, token usage where observed, and separate estimated
and actual costs. No prompt or response content is stored in this ledger.

`POST /v1/admin/provider-attempts/{id}/settlement` accepts
`{ "actual_cost_microusd": 7, "evidence": "invoice reference" }`. Settlement is
idempotent for identical evidence and amount, rejects conflicting repeats, and
retains the upstream outcome even for billed failed requests. It never changes
customer usage charges. Without reconciliation, an observed usage calculation
is `estimated`, and an unobserved/failed/cancelled charge remains `unknown`.

### Outstanding full-issue acceptance gates

Do not treat the management or transport-fixture tests as proof of full #1273.
Remaining work includes registry-authoritative import and rollback for all legacy
NEAR/long-context/Chutes/Tinfoil sources; non-token billing units; complete auxiliary
inference billing and attribution; public inference-response deployment IDs; and
process-level real-vendor and verifier-backed TEE acceptance evidence. Live acceptance requires provisioned
upstreams and credential references. The current mocks prove management and
transport behavior only.

### Staged legacy external import

`POST /v1/admin/providers/import-legacy` accepts `model_name`, `credential_env`,
`phase` (`plan`, `shadow`, `cutover`, or `rollback`), and `change_reason`.
Provision the credential reference on every replica first. An existing inline
key must match that reference. Import responses and audit snapshots never
contain the resolved key. Global-key legacy configurations require an explicit
reference, rather than silently borrowing a different account's adapter key.

`plan` validates without writing. `shadow` records stable deployment, inventory,
and route identities while the legacy configuration still serves; shadow routes
are excluded from serving. `cutover` requires the unchanged shadow snapshot and
makes registry routes authoritative for that model. It removes the inline key
from the current catalog configuration and retains a sanitized rollback config.
`rollback` restores the legacy loader using the credential reference. Identical
repeated transitions do not duplicate import audit records. Existing catalog
UUIDs, aliases, archives, activation, and customer prices are preserved. When
runtime configuration is identical, cutover and rollback reuse the existing
provider handle. This workflow covers external adapters only; it does not
migrate verifier-backed fleet or third-party provider constructors.

### Organization policy

`GET/PUT /v1/admin/organizations/{org_id}/inference-policy` manages a stronger
organization requirement. PUT accepts `tee_required` and `change_reason` and
records an audit row. Authentication reads this policy without a stale policy
cache and fails closed if the lookup fails. The authenticated dispatch scope
requires TEE regardless of caller `tee: any`. Responses carries this constraint
explicitly into background tasks and resolves omitted TEE defaults before the
tool loop, so task spawning or a later catalog edit cannot weaken it.

### Public route capabilities

`GET /v1/models`, `/v1/model/list`, and `/v1/model/{name-or-alias}` expose
`routing` (under `metadata` for the latter two). It contains the default TEE
requirement and route-specific profiles with stable registry route/deployment IDs,
TEE transport availability, native protocol support, and reasoning efforts.
Reasoning efforts are null when unknown and an empty list when explicitly
unsupported. Profiles disappear when routes are withdrawn or the registry
snapshot becomes stale; public HTTP caches can delay listing updates. These
capabilities describe configured transports, not verification of a particular
response. Actual verification still depends on the serving transport's verifier.

Usage rows and usage history/report APIs carry nullable `served_route_id` and
`served_deployment_id` for attributed registry dispatches. The billing function
records the pair atomically with the debit, validates that it belongs to the
billed model, and preserves it on duplicate submissions. Historical/legacy
rows remain null; no security or deployment identity is guessed retroactively.

Primary API inference billing carries the accepted catalog rate snapshot through
provider completion and asynchronous stream finalization. Usage rows retain it
in `billingDetails.accepted_catalog_pricing`; provider cost profiles cannot alter
those customer rates. Trusted external usage reporters retain the documented
current-catalog pricing behavior. Auxiliary auto-redaction and Responses image
billing still need full acceptance coverage.
