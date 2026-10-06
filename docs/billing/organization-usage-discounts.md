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
