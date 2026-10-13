# Usage credit allocation

Every newly posted inference or platform-service charge is split, in exact
integer nano-USD, across the organization's active credit limits. The default
order is:

`grant -> staking_farm -> payment -> postpay`

Set `CREDIT_USAGE_ORDER` to a comma-separated permutation of those four
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
