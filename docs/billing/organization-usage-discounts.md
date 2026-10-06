# Organization inference discounts

Administrators can read or save an organization discount at
`GET|PUT /v1/admin/organizations/{org_id}/usage-discount`.

The PUT body contains `discount_basis_points` (an integer from 1 to 10000).
Omit `apply_since` to affect only new usage. Supply an RFC3339 `apply_since`
timestamp to also correct inference usage from that inclusive timestamp through
the save-time boundary. GET returns `null` when no rule exists; otherwise both
endpoints return the percentage, optional cutoff, save time, status and processed
historical row count. Identical saves are idempotent; different second rules
return 409. Rules are intentionally immutable in this initial implementation.

The discount applies after existing model/reporter pricing. Input and output
costs round independently to nano-USD using half-up rounding; their sum is the
canonical billed total. The existing reporter snapshot is preserved, and a
separate `billing_details.contract_discount` block records the pre-contract
amounts. Platform-service charges are unaffected.

Historical corrections return 202 and remain `applying` while a persistent
background worker processes bounded batches. New inference is discounted
immediately. Each batch atomically corrects usage costs, balance, consumption
counters and hourly totals. Existing allocation entries stay unchanged; appended
credit-note reversals release the last posting-priority allocation first. This
releases capacity now and does not reassign later historical charges. Legacy
unattributed usage reduces the existing legacy counter instead. Unsupported
unfunded/settled historical usage is rejected before saving a rule.

The worker resumes its cursor after restart. A failed batch rolls back entirely;
it retries without compounding discounts. Status becomes `active` only after
accounting and reporting conservation checks pass. If progress stops, investigate
the worker error before refreshing downstream historical snapshots. A historical
aggregate mismatch can be repaired using the existing usage-hourly recompute API.

Once the rule is active, refresh downstream billing snapshots and historical
usage streams using their existing resync procedure. Compare matching measures
and time cutoffs; downstream systems should not apply a second percentage.

Before enabling any rule, finish the rolling deployment and verify that **every
API replica** runs this version. Older replicas do not apply the discount or read
credit-note reversals. Do not activate a rule during a mixed-version rollout.
After a rule has been saved, rolling back to an older billing implementation is
not supported: retain this accounting behavior and deploy a forward fix instead.

Workers claim due organizations with a persisted ten-minute lease. A claim is
fenced by a unique token, expires after a crash, and runs at most 270 seconds of
work. At most two jobs run per API replica; a transaction lock also prevents
concurrent final verification for the same organization. Each completed batch
rotates behind other due work. Failures retain `applying` status, show a safe
`last_error` and `next_retry_at`, and back off from 30 seconds up to 30 minutes.
Successful progress clears those diagnostics. Full accounting/reporting checks
remain required before activation; pre-existing drift must be repaired rather
than treating a partially reconciled organization as complete.
