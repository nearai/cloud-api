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
The optional filter accepts `grant`, `staking_farm`, `transfer`, `payment`, and `postpay`
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
- Inference usage posts through `record_organization_usage` (V0096) in one database
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
`PUT /v1/admin/organizations/{org_id}/policy`:

```json
{
  "policy": {
    "credentials": {"enabled": true},
    "credits_transfer": {
      "enabled": true,
      "max_sends_per_minute": 60,
      "constraints": {
        "max_amount_per_transfer": 10000000000,
        "max_total_amount": 500000000000,
        "allowed_source_types": ["payment", "staking_farm"]
      }
    }
  },
  "reason": "Reviewed opening accounting"
}
```

The policy uses named sections for future extension. PUT replaces the complete
supported policy; unknown fields and permissions are rejected, never silently
accepted. GET returns current policy and append-only audit. Read-only admin
credentials may inspect policies and reconciliation but cannot change policies.
No organization is enabled by migration. Disabling sends retains authorized
history and exact replay access. Organization-wide budgets include owner-session
sends and all credentials, including revoked credentials.

### Organization credentials

Owners manage organization-owned automation credentials. Administrators must
first enable `policy.credentials.enabled` (default false). The server gates
creation, configuration updates, and rotation; listing and revocation remain
available when disabled so owners can still remove existing access. This flag
does not revoke existing secrets or disable their use; use revocation or the
separate transfer policy to stop sending.

Cloud Admin UI exposes this organization policy alongside transfer settings.
Credential management controls require both this policy flag and owner membership;
being a platform admin alone does not grant permission to mint an organization's
credentials.

Endpoints:

| Method | Path (under `/v1/organizations/{org_id}`) | Purpose |
| --- | --- | --- |
| POST | `/credentials` | Create; secret returned once |
| GET | `/credentials` | List configuration, decimal-string lifetime transfer usage, and audit |
| PUT | `/credentials/{id}` | Replace configuration; preserve identity and usage |
| POST | `/credentials/{id}/rotate` | Replace secret immediately; preserve identity and usage |
| DELETE | `/credentials/{id}` | Revoke; repeated revocation preserves timestamp and audit |

Create or update with:

```json
{
  "name": "Contributor payouts",
  "permissions": ["credits:read", "credits:transfer"],
  "constraints": {
    "credits:transfer": {
      "max_amount_per_transfer": 10000000000,
      "max_total_amount": 500000000000,
      "allowed_source_types": ["payment"]
    }
  },
  "expires_at": "2027-01-01T00:00:00Z"
}
```

Amounts are integer nano-USD: the example permits $10 per transfer and $500 over
the credential's lifetime. `expires_at` must be in the next 366 days. Optional
constraints are `max_amount_per_transfer`, `max_total_amount`,
`allowed_source_types`, and `allowed_recipient_ids` (at most 1,000 UUIDs).
Omitting a constraint adds no restriction; an empty allowlist permits nothing,
and a zero monetary limit blocks new sends. Organization policy and credential
constraints both apply. Postpay and received transfer credits can never be sources. Grant requires an
explicit `grant` entry in the organization policy `allowed_source_types`; omitting
that policy allowlist permits only payment and staking. Credential restrictions
can narrow the policy but cannot enable a source the policy disallows.
A per-credential limit bounds an integration, not the owners who can create more
credentials; the organization policy is the aggregate budget boundary.

`credits:read` allows transfer history/detail and capacity reads. `credits:transfer`
allows sends and their original response replays; it does not implicitly grant
history reads. Neither permission authorizes credential management, policy
changes, or inference. Owners manage credentials using their sessions. Existing
reporting tokens and workspace inference API keys remain unchanged.

Secrets have the `oc_` prefix; only their SHA-256 hashes and safe display prefixes
are stored. Listing and audit never expose hashes or secrets. Unknown credential
IDs return 404. Creation, configuration changes, rotation, and revocation are
audited. Credentials belong to the organization: removing the creator does not
revoke them. Other owners must review and revoke credentials during offboarding.
Rotation immediately invalidates the old secret without resetting usage or expiry;
update expiry separately if needed. A revoked credential cannot be reactivated.

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
`?idempotency_key=...` for an outgoing lookup, or `/credit/transfers/{id}` for a
single record. Sender responses contain its original snapshot and delivery
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
or historical reconciliation. Consumption and reporting use type `transfer`.

Each transaction locks both organizations in UUID order. Current membership or
credential authority is checked under a row lock; membership changes and
credential revocation serialize against that check. Session authentication occurs
at HTTP admission. Policy and organization lifecycle updates use the same durable
organization locks. Existing inference admission does not reserve capacity: work
admitted before distribution can finish afterwards with overage. Further sends
are blocked once overage is posted. Prefer a dedicated distribution organization.

409 codes distinguish `insufficient_posted_capacity`, `sender_not_reconciled`,
`idempotency_conflict`, `capacity_overflow`, `organization_transfer_limit_exceeded`,
and `credential_transfer_limit_exceeded`. 403 distinguishes
`credential_refused` and `sender_not_enabled`. 404 `organization_unavailable`
debits nothing; recipient availability is checked only after sender authorization
and policy. By default, at most 60 new successful sends per sender per rolling minute are
allowed, configurable through organization policy (1–100,000); exact replays do not consume that quota. 429 uses `transfer_rate_limited`.
Transient database/lock errors return 503; retry with the original key.
`credit_transfer_operations` records bounded outcome labels, never user metadata.

### Deployment and reconciliation

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
4. Call `GET /v1/admin/organizations/{org_id}/credit/transfer/reconciliation`
   using an admin session or read-only admin token. It returns `consistent`,
   `checked_at`, and structured `findings` for ledger/projection mismatches,
   incoming-limit mismatches, outgoing above gross source capacity, and transfer
   consumption above incoming credits. Operators need no SQL access. This is a
   single consistent database snapshot, not an automatic repair or a guarantee
   against later manual changes. Investigate findings before enablement.
   Corrections remain separate reviewed maintenance operations; never rewrite
   completed transfers or historical usage allocations.
5. Enable one reviewed, fully reconciled, debt-free reseller and check end-to-end
   delivery, consumption attribution, and staking sync before broadening access.
6. Emergency rollback disables policies but retains transfer-aware accounting.
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
manual corruption is still detected by reconciliation. When setting a custom
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
Reconciliation reports `source_currency_mismatch` when non-USD capacity would
otherwise conceal missing USD backing. Non-USD limits remain visible in balance
snapshots but contribute zero to transfer capacity.

Legacy organization/user aggregate `spend_limit` responses are bounded at
`i64::MAX` when the sum of individual sources exceeds that range. Individual
source limits and the transfer ledger retain their exact amounts; the bounded
aggregate is a display value, not an authorization or accounting input.
