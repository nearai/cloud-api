# Usage credit allocation

Every newly posted inference or platform-service charge is split, in exact
integer nano-USD, across the organization's active credit ceilings. The default
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
current `available` nano-USD for every active credit type. Postpay remains a
cumulative contract ceiling and is kept separate from purchased payment
credits. Replacing a ceiling does not reset consumption: changing 100 to 150
adds 50 of capacity, while writing 150 again adds none.

Both usage-history endpoints and the reporting export/summary accept a
`credit_type` filter. Mixed charges are returned once; filtered cost is only the
matching allocation amount, while request and token counts are not multiplied.

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
- Existing unattributed usage remains unknown and continues reducing aggregate
  admission capacity. Its amount is snapshotted during the allocation-ledger
  migration so usage posting does not rescan lifetime history. Establish
  reviewed starting ceilings rather than backfilling speculative per-type
  allocations.
- Monitor allocation reconciliation and unfunded balances before expanding the
  rollout. Allocation rows and their parent usage/balance update commit in one
  transaction.
