# Default organization

Deploy migration V0081 and Cloud API before the corresponding UI change.
**Drain and stop signup traffic on all old API instances before running V0081**,
then start the updated API before re-enabling signup. Old registration writes
user and membership in separate transactions: a user between those writes is
indistinguishable from legacy missing history and would be marked `unresolved`.
This migration therefore requires a coordinated rollout, not a rolling deployment
with old signup writers. The updated registration commits user, organization,
owner membership, and workspace atomically, rolling back all of them on failure.

V0081 updates existing users and holds schema locks until commit. Measure user
and membership counts and rehearse on a representative database to budget the
maintenance window before production rollout.
`/v1/users/me` exposes `default_organization_id` and `default_organization_source`.
Consumers must not fall back to list order when the ID is null or unavailable.
The response includes an accessible default even when it is outside the first
100 organizations; the list can therefore contain 101 entries.

The designation is persisted when a new user's first membership is inserted,
in the same transaction as the complete registration. A BEFORE INSERT trigger
serializes joins per user and assigns `joined_at` using `clock_timestamp()` after
acquiring the lock. Live inserts cannot backdate membership timestamps; existing
historical timestamps are preserved by the backfill. Neither
later invitations, membership removal nor ownership transfer changes it.
Deletion and deactivation are blocked for any user's designated organization,
including inactive users and users who no longer have membership. The API returns
HTTP 409 with error type `default_organization`. Database triggers also enforce
the restriction for other write paths.

## Legacy data

V0081 backfills from all retained memberships, including inactive organizations,
ordered by `joined_at ASC, organization_id ASC`. This is independent of ownership,
organization creation time and pagination. The source is explicitly reported as
`earliest_retained_membership`, not a claim that the original signup membership
still exists. Physically removed memberships are not recoverable from the current
schema. Audit/backup history must be used to establish the original in such cases;
the migration does not invent that history.

Users without retained membership are marked `unresolved` with a null ID and
remain unresolved on later joins. Inactive designated organizations remain
inactive and do not get replaced with a different active organization. Newly
inserted users awaiting their first membership have source `pending`; successful
assignment changes it to `first_membership`.

Repair of legacy history requires a deliberate database migration backed by
verified history, explicitly accounting for the designation protection trigger.
There is no automatic reassignment or user-facing reset operation.

## Regression checks

Run `cargo test -p database --test default_organization` against a disposable
PostgreSQL database using the `PGHOST`, `PGPORT`, `PGUSER`, and `PGDATABASE`
environment variables. The backfill test is standalone:
`psql -v ON_ERROR_STOP=1 -f crates/database/tests/sql/default_organization_backfill.sql`.
API coverage lives in `e2e_all::organization_deletion`.
