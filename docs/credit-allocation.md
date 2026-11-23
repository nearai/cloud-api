# Usage credit allocation

Every newly posted inference or platform-service charge is split, in exact
integer nano-USD, across the organization's active credit limits. The default
order is:

`grant -> staking_farm -> transfer -> payment -> postpay`

Set `CREDIT_USAGE_ORDER` to a comma-separated permutation of those five
API/database names to change the order. Set
`CREDIT_ALLOCATION_POLICY_VERSION` to a new non-empty identifier whenever the
policy meaning changes. The configured version and the active limit/source are
saved on each allocation; configuration changes never rewrite prior usage or
idempotent retries.

One usage row can therefore contain several `credit_allocations`. At posting,
their positive `amount` values plus `unfunded_amount` equal `total_cost`.
Later settlement allocations reduce the effective unfunded remainder by the
same amount, so usage responses remain reconciled without rewriting the
original usage row. Zero-cost usage has an empty allocation list and zero
unfunded cost. Historical rows created before attribution omit these fields
rather than inventing a funding history.

The organization balance endpoint reports `amount`, lifetime `consumed`, and
current `available` nano-USD for every active credit type. It also returns
`legacy_unattributed_amount` from the stored balance, including when no credit
type is active. This is historical consumption still awaiting attribution,
separate from `unfunded_amount` (usage exceeding capacity). Postpay remains a
cumulative contract limit and is kept separate from purchased payment
credits. Replacing a limit does not reset consumption: changing 100 to 150
adds 50 of capacity, while writing 150 again adds none.

Both usage-history endpoints and admin organization
metrics (`/v1/admin/organizations/{org_id}/metrics`) accept a
`credit_type` filter. Mixed charges are returned once; filtered cost is only the
matching allocation amount, including later settlement allocations, while request
and token counts are not multiplied. Each matching request contributes its full
token counts once; request/token counts are not additive across credit types.
Unattributed historical usage is excluded when filtering and retained when the
filter is omitted.

Admin metrics retain their existing inference-usage scope (not platform-service
usage), response shape, and half-open `[start, end)` usage timestamp range.
Settlement amounts belong to the original usage date, not the settlement date.
A period's per-credit totals may therefore increase on later queries until its
outstanding unfunded usage is settled; these are not frozen invoice totals.
Filtered admin metrics share a 10-second database-query budget across their
statements, using transaction-local timeouts. Unfiltered requests keep their
existing timeout behavior.
The optional filter accepts `grant`, `staking_farm`, `transfer`, `payment`, and `postpay`
(case-insensitively); unsupported values return HTTP 400.
For example, September postpay consumption can be queried with
`/v1/admin/organizations/{org_id}/metrics?start=2026-09-01T00:00:00Z&end=2026-10-01T00:00:00Z&credit_type=postpay`.
This reports consumed usage, not invoices, payment status, or cash received.

If a completed request costs more than all available capacity, the full charge
is retained with an `unfunded_amount`. Admission checks block subsequent usage
until that debt is resolved. When capacity is later added or re-enabled from
any source, it automatically funds the oldest overage first in the configured
priority order. Settlement appends immutable allocation rows with the current
limit, source, and policy version; it never edits the original posting-time
allocations. If the added capacity exceeds the debt, the remainder is
immediately available for new usage. No admin API call is required.

## Estimated admission

Chat/Completions, Responses, Messages, image generation/edit, audio transcription
and SystemOne reserve a local cost estimate before upstream dispatch, for every
organization. Admission atomically checks both organization capacity and a key's
optional spend limit. A key without a limit still shares the organization budget.
The actual balance is unchanged; spendable capacity excludes active holds. Final
settlement posts the full actual charge and deactivates the hold in the same
transaction. The original estimate is never resized, debited or refunded. Admission
creates an active hold in one operation. The internal release operation only frees
attempts confirmed not to have been dispatched. It stores the amount, inference type and estimator version with
billing identity and expiry, without persisting full input features or estimator policies.
The quote uses the prices resolved for admission. The first successful posting uses
the prices and organization discount effective at settlement. A duplicate report with unchanged usage facts
returns the recorded charge even when live prices or discounts have since changed.

The administrator-only `usage_admission` setting controls admission. Its default
organization policy is `audit`: estimates are recorded without creating holds or
rejecting requests on admission errors. Actual usage is billed in every mode.

An instance needs one successful policy load before it can admit requests. If the first
load fails, admission returns HTTP 503 and the instance retries every 30 seconds.
After a successful load, failed refreshes keep the last policy. An empty set of stored
settings is a successful load and uses the defaults.

Cloud API loads organization-specific overrides with the global policy. Each
unset field inherits its value from the global `defaults`. An organization creates
holds when its effective mode is `enforce` and admission is enabled.

The top-level `enabled` switch defaults to `true`, which uses the selected organization
policy. Setting it to `false` skips admission for every organization, including explicit
overrides. Re-enabling restores those policies. Settings take effect through the existing
reload mechanism. This switch does not cancel work or release existing holds.

`enforce` estimates and reserves; `audit` estimates without holding credit; `off`
skips admission. Authentication, existing credit checks and actual billing remain active.
Changing a mode does not release previously created holds. Limits must be positive;
the normal TTL is 1–86400 seconds and the unknown-outcome TTL must be positive and no
longer than the normal TTL. Invalid settings are rejected.

After the global settings load, an invalid enforce policy or an unreadable override
returns 503 for that organization alone. A known audit or off mode can continue
with default limits when its numeric overrides are invalid. Global disable applies
to every organization.

Audit still performs local feature measurement, reads model pricing, and records estimates.
It avoids reservation writes and admission row locks. Feature collection precedes policy
selection and remains on the request path when admission is disabled. Direct admission still
makes its control request in audit mode, and a disabled policy still returns a response to it.

Estimator parameters live in
[`budget/policy.rs`](../crates/services/src/usage/budget/policy.rs) and ship with
Cloud API. `ESTIMATOR_VERSION` identifies their calculation; the current version is 3.

| Estimate parameter | Default |
|---|---:|
| `margin_basis_points` | 5000 (50% margin) |
| `minimum_nano_usd` | 1000 |
| `bytes_per_token` | 3 |
| `default_output_tokens` | 4096 |
| `image_input_tokens` / `audio_input_tokens` | 2048 / 8192 |
| `video_input_tokens` / `unknown_input_tokens` | 16384 / 16384 |
| `audio_bytes_per_second` / `default_audio_seconds` | 1024 / 60 |
| `decision_output_tokens` | 128 per SystemOne question |

Text estimates count local UTF-8 bytes, complete tool definitions and structured
output schemas (including property names), explicit token IDs, a flat 32-token overhead and
media allowances, then add the requested output estimate capped by the known catalog
output limit. Without an explicit limit, the quote uses the known catalog output limit,
or 4096 only when the catalog has no limit. These are estimates, not hard ceilings
on actual cost. If several in-flight requests finish above their estimates, each settles
in full and their overages can accumulate. Conversely, an estimate above available
credit returns HTTP 402 before dispatch. A smaller output allowance can reduce the
quote even when the input is unchanged. The eventual cost of a refused request is
unobserved. To evaluate overestimation, sample completed audit requests and compare
the quote, the credit available at admission, and the posted charge. Fixed per-item
media allowances also apply to opaque or remote media;
no downloads or remote counting are performed.
Batch/choice/attempt counts multiply the corresponding units. Images use requested
image count and catalog price; transcription uses the larger of the byte/rate floor
and locally measured duration (or the 60-second allowance when duration is unavailable). Reservations add a 50% margin, rounded up to the next integer nano-USD,
then apply the minimum to nonzero estimated cost. For a single plain-text chat
generation on a vLLM route that enforces the requested output cap, only estimated
input cost receives the margin. Other requests apply the margin to the full estimate.
Free prices remain free.
Tiered models quote the requested service tier (default when omitted), using its highest
context-band and cache-class rates. Unknown or unconfigured tiers conservatively use
the highest configured rates; pricing fallback does not alter the upstream request.
No provider token-count call, media download, transcoding, or additional output
limit is introduced. Existing inference-side parameter handling remains in place,
including native Responses' existing catalog output cap. E2EE follows the same
admission path using locally visible sizes/counts; content is not sent to the ledger.

These values control exposure, not the final bill or a guaranteed per-request
maximum. Actual usage above the estimate is fully posted, with unfunded debt when
needed. Funding a charge, transferring credits, replacing limits and reconciling
older debt all protect other requests' active holds under the organization lock.

Each actual retry gets its own reservation. Actual usage follows the existing
recording and retry paths; admission adds no durable receipt queue or background
settlement replay. The background expiry pass runs every 30 seconds and visits up to 100 organizations.
It skips locked organizations, continues after an individual cleanup error, and resumes
after the last organization it attempted. A busy organization can wait multiple passes.
Admission also reclaims expired holds for its organization. There are no heartbeats or estimate resizing. A client disconnect or transport
error still records any billable usage already received and settles the hold.
A connection or request-construction failure known to precede dispatch releases the
hold immediately. If release fails, the service attempts to apply the shorter unknown-outcome grace period. HTTP errors, post-connect timeouts, cancellation and missing final usage
mark the outcome unknown: the attempt leaves the running-slot count, while its money
stays reserved until the earlier of its original expiry and the configured grace period
(default five minutes). Duplicate notices never extend that deadline. The same treatment
applies when actual-usage recording fails; existing receipt retries continue.
Cancellation during initial dispatch uses a best-effort drop guard. Process crashes or
failed lifecycle notifications still rely on the original TTL. Response completeness
remains a separate requirement for signing. Expiry frees both reserved credit
and the unsettled slot, preserving the identity needed for a late usage report.
A late report still charges actual usage once, including overage. Work that outlives
the TTL may overlap newly admitted work.
Duplicate reports charge once even after a lost acknowledgement or deletion of the
optional Responses record. Released and settled rows are retained. Active-only indexes
support expiry and admission as history grows. Late receipts need the reservation and
its billing identity. Duplicate receipts need the link to the original usage row and
its posted charge. Any retention limit would have to preserve these lookups or define
a maximum receipt-delivery window. If final usage is lost before it reaches
the existing accounting path, expiry does not reconstruct it or charge the estimate.

Audio settlement prefers the provider's duration-based `usage.seconds`, then
verbose-response `duration`. Off mode skips local duration parsing. Uploaded-file timing and byte counts are admission
estimates only and never substitute for actual usage. Missing provider duration
returns a billing error from the gateway; direct proxy reporting logs the missing
usage without charging an estimate. The reservation uses the unknown-outcome grace period.

Optional local metering has at most four workers and a one-second parsing budget.
MP3 metering counts complete frames instead of trusting summary frame counts.
WebM skips local parsing and uses the byte-based estimate. Uploads, response format,
and encryption are preserved.

Direct proxy requests use the internal admission endpoint with numeric metadata and a sanitized service-tier label
and a separate `admission_id` in the final HTTP/SQS usage receipt. Admission also
authorizes execution once: replaying an admitted ID does not authorize another
inference. Direct proxies attempt release by their generated ID after a lost admission
acknowledgement, before any inference dispatch. Cleanup racing an uncommitted admission
can miss the row, so the TTL remains the fallback. Each new upstream attempt needs a
fresh ID and reservation. Direct admission defaults to a two-second RPC deadline;
the database admission transaction has a 1.5-second deadline (including pool wait),
a one-second statement timeout and a 500ms lock timeout. Cloud API-mediated
requests reserve locally and are not reserved again by the proxy. Embedding,
rerank, privacy, score and standalone MCP search retain their prior admission
flow; their ledger writes still respect active holds.

## Accounting compatibility

Admission and settlement require compatible reservation tables and accounting operations.
All debit paths must respect active holds. Inference usage posts atomically through
`record_organization_usage`; service usage retains its transaction path. Allocation rows
and the parent usage/balance update commit together. Existing unattributed usage continues
to reduce aggregate admission capacity.

## Reseller credit transfers

Each reseller is enabled through its organization policy. No organization is
enabled by migration. This service no longer exposes routes to create or change
policies or organization credentials: policies and credentials that already exist
stay in effect for the transfer routes below. Disabling sends retains authorized
history and exact replay access. Organization-wide budgets include owner-session
sends and all credentials, including revoked credentials.

### Organization credentials

An organization credential authenticates transfer routes with
`Authorization: Bearer oc_...`. Each has a name, permissions (`credits:read`,
`credits:transfer`), optional constraints and an expiry within 366 days of
creation. Optional constraints are `max_amount_per_transfer`, `max_total_amount`,
`allowed_source_types`, and `allowed_recipient_ids` (at most 1,000 UUIDs).

Amounts are integer nano-USD. Omitting a constraint adds no restriction; an empty allowlist permits nothing,
and a zero monetary limit blocks new sends. Organization policy and credential
constraints both apply. Postpay and received transfer credits can never be sources. Grant requires an
explicit `grant` entry in the organization policy `allowed_source_types`; omitting
that policy allowlist permits only payment and staking. Credential restrictions
can narrow the policy but cannot enable a source the policy disallows.
A per-credential limit bounds an integration; the organization policy is the
aggregate budget boundary.

`credits:read` allows transfer history and capacity reads. `credits:transfer`
allows sends and their original response replays; it does not implicitly grant
history reads. Neither permission authorizes inference. Workspace inference API
keys are unchanged.

Secrets have the `oc_` prefix; only their SHA-256 hashes and safe display prefixes
are stored. Credentials belong to the organization: removing the creator does not
revoke them. A revoked credential cannot be reactivated.

Lifetime usage is derived from the immutable transfer ledger under the same
organization lock as sends. Failed operations and exact retries consume no budget.
Raising or lowering a budget never resets usage; lowering below spent blocks new
sends but permits authorized exact retries. Authorization is still required for a
retry: expired, revoked, or permission-removed credentials cannot replay.

Send `POST /v1/organizations/{sender_org_id}/credit/transfer` with an owner session
or `Authorization: Bearer oc_...`, plus a required `Idempotency-Key` header:

```json
{
  "recipient_organization_id": "explicit-recipient-organization-uuid",
  "source_credit_type": "payment",
  "amount": 30000000000,
  "memo": "Order reference",
  "metadata": {"order_id": "opaque-reference"}
}
```

Amounts are positive integer nano-USD. The explicitly chosen `payment`, already-posted `staking_farm`, or explicitly
admin-approved `grant` source funds a send. Postpay, future staking rewards, and
received `transfer` capacity cannot fund it. Distributed grants arrive as ordinary
nontransferable `transfer` capacity, preserving the single-hop rule. Staking source
limits must come from `house-of-stake`. Sync externally accrued rewards separately
and retry if posted capacity is insufficient. Network operations never run inside
the transfer transaction. Evidence reports the sync observation time separately
from the chain accrual cutoff (currently unknown/null).

For cumulative payouts, omit `amount` and `Idempotency-Key` and provide a named
program and nonnegative target in nano-USD:

```json
{
  "recipient_organization_id": "explicit-recipient-organization-uuid",
  "source_credit_type": "payment",
  "payout_program": "partner-rewards-2026",
  "recipient_cumulative": 100000000000
}
```

The total is scoped to **sender + recipient + payout program**, across credentials
and funding sources. Program names are exact, nonempty strings of at most 100
bytes, with no surrounding whitespace. An amount-based send may include the
same `payout_program` and will count toward that total. Untagged sends do not
count toward any program. Use a new program name for a separate payout schedule;
renaming a program starts an independent total.

Under the organization accounting locks, the server calculates the positive
difference between the target and all completed transfers in that program. Only
that difference consumes source capacity, budgets, and send quota. Concurrent
or out-of-order targets cannot overpay the highest accepted target. A reached or
lower target returns 200 with `amount: 0`, `no_op: true`, and the current
`recipient_cumulative`; it creates no ledger row and consumes no quota. A positive
difference returns 201 and records the actual amount and resulting program total.
Cumulative calls require current transfer authorization, an active recipient,
and an enabled organization source policy, including on no-ops. Unlike an exact
amount replay, they evaluate the current target instead of replaying an earlier
response. Retrying the same target after a timeout is safe without an idempotency
key; supplying a key in cumulative mode is rejected to avoid mixed retry semantics.

Creation returns 201; an exact replay returns 200 with the original result even
if balances or sender policy have since changed. Keys are trimmed, nonempty,
and at most 100 characters, scoped to the sender and retained with the ledger.
Failures before commit, including unavailable recipients, do not reserve keys or
debit either organization. Use one stable key per intended transfer and reuse the
original terms after a timeout. Clients/SDKs may automatically generate a UUID
once before the first attempt and persist/reuse it across retries. A new server-
generated UUID per request cannot deduplicate a committed transfer whose response
was lost. The server separately generates the transfer record ID.
Changes to any business field return 409.

Memo defaults to the empty string, preserves whitespace, and permits at most
2,000 UTF-8 bytes. Metadata defaults to an object and permits at most 16 KiB of
compact UTF-8 JSON and 16 nesting levels. Explicit nulls and non-object metadata
are rejected. Object keys are sorted recursively for storage; arrays retain order.
Idempotency uses PostgreSQL JSONB structural equality, including numeric value
equality, so object key order and whitespace do not cause conflicts. Both parties
can read memo and metadata; do not put secrets in them, and render memo as plain
text. Billing retention applies to these immutable records and actor IDs, including
after account erasure or organization deactivation.

`GET /v1/organizations/{org_id}/credit/transfers` lists authorized incoming and
outgoing transfers, newest first, with a maximum page size of 100. Select
`?direction=outgoing` or `?direction=incoming` to restrict the list. Pass both
`before_time` and `before_id` from `next_cursor` for the next page. Use
`?idempotency_key=...` for an outgoing lookup. Sender responses contain its original snapshot and delivery
confirmation; only recipient views contain its snapshot and debt settlement.
`GET /v1/organizations/{org_id}/credit/transfer/capacity` requires `credits:read`
or an owner session. It returns own credit positions, posted own staking earnings,
reconciliation status, policy enablement, credential transfer permission,
organization/credential remaining lifetime budgets, remaining send quota, and a
per-source maximum for one new transfer. The maximum accounts for both budgets,
per-transfer ceilings, permissions, policy, and source eligibility. Recipient
allowlists are returned in the constraints and must also be satisfied; recipient
availability is checked at send time. This snapshot is informational, never a
reservation. A subsequent concurrent send, usage posting, or configuration change
can change capacity before the requested transfer executes.

Balance breakdowns retain gross `amount` semantics and add `transferred_out` and
`effective_capacity`. Availability is `max(gross - outgoing - consumed, 0)` for
active sources. Aggregate legacy deductions still apply, and unreconciled or
indebted senders cannot send. Transfers do not increase lifetime consumption,
usage counts, gross own staking earnings, payment limits, or cash receipts.
Received credits are system-owned and cannot be created by general limit updates
Consumption and reporting use type `transfer`.

Each transaction locks both organizations in UUID order. Current membership or
credential authority is checked under a row lock; membership changes and
credential revocation serialize against that check. Session authentication occurs
at HTTP admission. Policy and organization lifecycle updates use the same durable
organization locks. Covered inference endpoints reserve capacity before dispatch,
and transfers exclude those active holds. Actual usage above an estimate and
endpoints without admission can still post overage; further sends are blocked
once overage is posted. Prefer a dedicated distribution organization.

409 codes distinguish `insufficient_posted_capacity`, `sender_not_reconciled`,
`idempotency_conflict`, `capacity_overflow`, `organization_transfer_limit_exceeded`,
and `credential_transfer_limit_exceeded`. 403 distinguishes
`credential_refused` and `sender_not_enabled`. 404 `organization_unavailable`
debits nothing; recipient availability is checked only after sender authorization
and policy. By default, at most 60 new successful sends per sender per rolling minute are
allowed, configurable through organization policy (1–100,000); exact replays do not consume that quota. 429 uses `transfer_rate_limited`.
Transient database/lock errors return 503; retry with the original key.
`credit_transfer_operations` records bounded outcome labels, never user metadata.

### Deployment

1. Ship the transfer-aware image and its five-type `CREDIT_USAGE_ORDER` together
   in the same deployment revision, with allocation policy version `v2` (or a new
   explicitly chosen version). New binaries reject four-type overrides and old
   binaries reject five-type overrides. Do not update a shared ConfigMap that old
   pods may reload/restart from. Deploy migration V0096 and every transfer-aware
   accounting writer before enabling any reseller. V0096 is an unreleased draft
   migration; environments that ran an earlier draft require a fresh test database.
2. Drain incompatible workers and update clients that assume four types. Choose
   the transfer priority deliberately; historical allocation versions remain
   unchanged. Pin both order and policy version together for custom policies.
3. Balance rendering can show gross, distributed, consumed, and available amounts.
   `effective_capacity` is a convenience field derived from gross minus outgoing;
   clients do not have to display it separately. This server PR does not deploy UI.
4. Enable one reviewed, fully reconciled, debt-free reseller and check end-to-end
   delivery, consumption attribution, and staking sync before broadening access.
5. Emergency rollback disables policies but retains transfer-aware accounting.
   Never roll back to binaries that ignore outgoing deductions or transfer credits.

Reversals, chargebacks, fees, expiry, claims by email, bulk sends, automatic
recipient selection, and per-reseller attribution of recipient usage are excluded.

Recipient spending attribution is intentionally deferred. Transfer history proves
who funded a recipient and each payout program's delivered total; it does not prove
which sender funded a later inference request. Recipients currently share one
transfer credit pool. A follow-up needs immutable funding lots linked to transfer
IDs, a documented consumption ordering rule across senders/programs, allocation
records linking usage to those lots, and privacy-aware reporting. It must preserve
existing historical allocations and the one-active-transfer-limit invariant.

Source limit replacement rejects a gross limit below credits already distributed
from that source. Reducing available capacity must retain that backing; external
manual corruption is not repaired automatically. When setting a custom
`CREDIT_ALLOCATION_POLICY_VERSION`, explicitly set `CREDIT_USAGE_ORDER` too so an
old policy version cannot silently acquire the new five-type default order.

Exact amount replays require current transfer authorization and either the original
credential identity or `credits:read` permission (owners have both permissions).
Rotation preserves credential identity. A transfer-only credential cannot retrieve
another actor's stored response. Organization-wide idempotency keys still reveal
key existence through `idempotency_conflict` when terms differ. Valid replays do
not require transfers to remain enabled or the original budgets to remain available.

Transfer source backing is denominated in USD. A source with distributed credits
cannot be replaced by a non-USD limit, even if its numeric amount is unchanged.
Non-USD limits remain visible in balance
snapshots but contribute zero to transfer capacity.

Legacy organization/user aggregate `spend_limit` responses are bounded at
`i64::MAX` when the sum of individual sources exceeds that range. Individual
source limits and the transfer ledger retain their exact amounts; the bounded
aggregate is a display value, not an authorization or accounting input.
