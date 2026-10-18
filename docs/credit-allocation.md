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

Both usage-history endpoints, the reporting export/summary, and admin organization
metrics (`/v1/admin/organizations/{org_id}/metrics` and `/metrics/timeseries`) accept a
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
The optional filter accepts `grant`, `staking_farm`, `payment`, and `postpay`
(case-insensitively, like reporting); unsupported values return HTTP 400.
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

## Rollout notes

- Deploy every inference, service, limit, and staking writer together. They
  coordinate through the same per-organization database lock.
- Inference usage posts through `record_organization_usage` (V0088) in one database
  statement; the organization lock is held only during server-side accounting.
  Platform-service usage still uses the Rust transaction path and the same lock.
- Existing unattributed usage remains unknown and continues reducing aggregate
  admission capacity. Its amount is snapshotted during the allocation-ledger
  migration so usage posting does not rescan lifetime history. Establish
  reviewed starting limits rather than backfilling speculative per-type
  allocations.
- Monitor allocation reconciliation and unfunded balances before expanding the
  rollout. Allocation rows and their parent usage/balance update commit in one
  transaction.

## Reconciling legacy consumption when moving to postpay

For an existing organization, review historical credit/payment/contract records
before choosing a credit type for its pre-attribution consumption. Admins can
reclassify a reviewed portion of `legacy_unattributed_amount` using:

`POST /v1/admin/organizations/{org_id}/credit/reconciliations`

```json
{
  "idempotency_key": "opening-review-2026-10",
  "amounts": { "payment": 80000000000 },
  "reason": "Verified $80 prepaid consumption against billing records",
  "dry_run": true
}
```

All amounts are positive integer nano-USD. Supported names are `grant`,
`payment`, `staking_farm`, and `postpay`. A nonempty reason (up to 2000 characters)
and a per-organization idempotency key (up to 100 characters) are required.
The endpoint requires an admin session or read-write admin access token;
read-only tokens cannot preview or commit this operation.

A dry run returns before/after per-type `spend_limit`, consumed and available amounts,
legacy unknown consumption, unresolved overage, and aggregate available capacity.
It stores nothing and does not reserve the key. Set `dry_run` to `false` (or omit
it) to commit; accounting is revalidated under the organization lock, so a
preview is not a reservation of its displayed position. Record the returned
`reconciliation_id` with the reviewed evidence. Identical committed terms with
an existing key return the original snapshots even after later usage; conflicting
terms return HTTP 409. A preview using an already committed key also returns 409.
Keys are trimmed once before lookup and storage, so whitespace around the key
cannot create a second adjustment. Snapshots include `schema_version`; additive
changes to persisted snapshot structs must use serde defaults and incompatible
versions need an explicit replay adapter. Snapshots and API responses use
`spend_limit`. Consumption without an active limit
is included with `active: false`, zero limit and zero available capacity.

The operation adds reviewed historical consumption to the existing per-type
counters and removes exactly the same amount from the aggregate legacy
consumption deduction. It preserves total spend, unresolved overage, current
limits, and aggregate available capacity. Each selected type must have an
active USD limit with enough remaining capacity; otherwise the operation
returns 409 and the opening position needs further review. Never increase a
limit merely to bypass this validation. The supplied amounts are increments,
not absolute replacements for lifetime consumption.

Use this order when moving an existing organization to postpay:

1. Finish historical discount corrections and review which funding types paid
   for the legacy consumption. Ensure the reconciliation-aware writers are deployed.
2. Ensure every selected type has an active USD limit with enough remaining
   capacity. If assigning history to `postpay`, create the approved postpay limit
   first. Keep existing grant/payment limits in place until their history has
   been reconciled; do not lower them first or inflate limits to bypass validation.
3. Dry run the reviewed increments and inspect the before/after balances.
4. Commit those same increments and reason with the same idempotency key, then
   save the audit ID. If live usage causes a conflict, review fresh balances and
   preview again before committing.
5. Apply the remaining approved limit changes, including reducing old grant or
   payment limits. Verify sufficient capacity remains for ongoing usage throughout
   the transition; reconciliation itself neither creates nor removes capacity.

For example, with a payment limit of $100, postpay limit of $200 and $80 of
legacy unknown spend, reclassifying the $80 as payment-funded leaves payment
capacity $20 and postpay capacity $200, with aggregate capacity unchanged at
$220. New postings read the corrected counters using the configured priority.
The endpoint does not change that priority or a contract's effective date.

Adjustments are stored in a separate append-only audit ledger with the actor,
reason, amounts and original before/after result. They do **not** rewrite old
usage logs, create per-request allocations, or make a postpay-filtered historical
report complete. Unreviewed amounts remain unknown; invoice settlement and
historical request-level attribution are separate operations.
Usage already allocated per request since the allocation ledger was introduced
keeps its recorded credit type. For example, usage allocated to `grant` while a
credit line was booked as a grant remains grant consumption after switching to
postpay. This is intentional for this endpoint: it only assigns currently
unattributed legacy consumption. Correcting previously attributed grant usage
would require a separate audited reclassification operation and is outside this PR.

Partial reconciliation preserves the remaining unknown deduction but cannot
establish a fully verified per-type opening position. Resolve all relevant
legacy consumption before relying on future allocations as verified funding.

Finish historical discount corrections before reconciling. A reconciliation is
rejected while a discount worker is applying. After a reconciliation exists,
new historical discounts are rejected because aggregate opening classifications
cannot safely be reversed against individual usage rows. Future-only discounts
remain supported. Existing fully completed discount corrections can be followed
by a reconciliation of their remaining legacy spend.

Complete the rollout of every usage-discount/admin writer before performing any
reconciliation. Old pods do not enforce the reconciliation guard and could
still accept an incompatible historical discount during a rolling deployment.
The audit retains the authenticated admin UUID without a users foreign key so
account deletion does not erase or rewrite the original actor. Audit text and
JSON fields are explicitly classified as restricted operational billing data,
consistent with limit-change reasons and billing correction records.


## Reseller credit transfers

An administrator explicitly enables each reseller with
`PUT /v1/admin/organizations/{org_id}/credit/transfer-policy`, sending
`{"enabled": true, "reason": "Reviewed opening accounting"}`. GET returns the
current policy and append-only audit. Read-only admin credentials cannot change
policies. No organization is enabled by migration. Disabling a policy stops new
sends but retains authorized history and exact replay access.

An owner session can create an organization-bound `credits:transfer` credential
with `POST /v1/organizations/{org_id}/credit/transfer-credentials`, passing an
`expires_at` timestamp in the next 366 days. The secret is returned once; only its
SHA-256 hash is stored. Rotate by issuing a replacement and deleting the old
`/credit/transfer-credentials/{id}`. These credentials cannot manage policies,
issue credentials, run inference, or read another organization's finances.
Ordinary inference API keys and reporting tokens cannot transfer credits.

Send `POST /v1/organizations/{sender_org_id}/credit/transfer` with an owner session
or `Authorization: Bearer ct_...`, plus a required `Idempotency-Key` header:

```json
{
  "recipient_organization_id": "explicit-recipient-organization-uuid",
  "source_credit_type": "payment",
  "amount": 30000000000,
  "memo": "Order reference",
  "metadata": {"order_id": "opaque-reference"}
}
```

Amounts are positive integer nano-USD. Only the explicitly chosen `payment` or
already-posted `staking_farm` source can fund a send. Grants, postpay, future
staking rewards, and received `transfer` capacity cannot fund it. Staking source
limits must come from `house-of-stake`. Sync externally accrued rewards separately
and retry if posted capacity is insufficient. Network operations never run inside
the transfer transaction. Evidence reports the sync observation time separately
from the chain accrual cutoff (currently unknown/null).

Creation returns 201; an exact replay returns 200 with the original result even
if balances or sender policy have since changed. Keys are trimmed, nonempty,
and at most 100 characters, scoped to the sender and retained with the ledger.
Failures before commit, including unavailable recipients, do not reserve keys or
debit either organization. Use one stable key per intended transfer and reuse the
original terms after a timeout. Changes to any business field return 409.

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
`?idempotency_key=...` for an outgoing lookup, or `/credit/transfers/{id}` for a
single record. Sender responses contain its original snapshot and delivery
confirmation; only recipient views contain its snapshot and debt settlement.
`GET /credit/transfer-capacity` returns the organization's own credit positions,
`own_staking_earned_total` from posted issuance history, and reconciliation status.
Source availability is transferable only while the sender is enabled and reconciled.

Balance breakdowns retain gross `amount` semantics and add `transferred_out` and
`effective_capacity`. Availability is `max(gross - outgoing - consumed, 0)` for
active sources. Aggregate legacy deductions still apply, and unreconciled or
indebted senders cannot send. Transfers do not increase lifetime consumption,
usage counts, gross own staking earnings, payment limits, or cash receipts.
Received credits are system-owned and cannot be created by general limit updates
or historical reconciliation. Consumption and reporting use type `transfer`.

Each transaction locks both organizations in UUID order. Current membership or
credential authority is checked under a row lock; membership changes and
credential revocation serialize against that check. Session authentication occurs
at HTTP admission. Policy and organization lifecycle updates use the same durable
organization locks. Existing inference admission does not reserve capacity: work
admitted before distribution can finish afterwards with overage. Further sends
are blocked once overage is posted. Prefer a dedicated distribution organization.

409 codes distinguish `insufficient_posted_capacity`, `sender_not_reconciled`,
`idempotency_conflict`, and `capacity_overflow`. 403 distinguishes
`credential_refused` and `sender_not_enabled`. 404 `organization_unavailable`
debits nothing; recipient availability is checked only after sender authorization
and policy. At most 60 new successful sends per sender per rolling minute are
allowed; exact replays do not consume that quota. 429 uses `transfer_rate_limited`.
Transient database/lock errors return 503; retry with the original key.
`credit_transfer_operations` records bounded outcome labels, never user metadata.

### Deployment and reconciliation

1. Deploy migration V0095 and every transfer-aware accounting binary before
   enabling a reseller. Migration introduces new operator-owned ledger, policy,
   credential, and projection data that cannot be represented by existing columns.
2. Explicitly migrate configured `CREDIT_USAGE_ORDER` from four to five types.
   Existing four-type overrides fail validation instead of being silently reordered.
   Choose the transfer position deliberately and bump the policy version (default
   `v2`). Historical allocation versions remain unchanged.
3. Drain incompatible workers and update clients that assume four types. Frontend
   balance rendering should show gross, distributed, consumed, effective, and
   available amounts separately; this server PR does not deploy those clients.
4. Run `scripts/reconcile-credit-transfers.sql`. It verifies ledger/projection
   agreement, incoming system limits, and global conservation. Investigate any row
   or nonzero difference before enablement. Projections can be rebuilt from the
   grouped ledger sums under accounting locks in a reviewed maintenance operation;
   never rewrite completed transfers or historical usage allocations.
5. Enable one reviewed, fully reconciled, debt-free reseller and check end-to-end
   delivery, consumption attribution, and staking sync before broadening access.
6. Emergency rollback disables policies but retains transfer-aware accounting.
   Never roll back to binaries that ignore outgoing deductions or transfer credits.

Reversals, chargebacks, fees, expiry, claims by email, bulk sends, automatic
recipient selection, and per-reseller attribution of recipient usage are excluded.
