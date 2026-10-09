# Refresh-token grace rollout

Deploy in this order; do not start the new API code before the index is ready:

1. While the old API still serves traffic, run the candidate image with the
   same database configuration as production but invoke `./api --migrate-only`.
   It applies V0088 (two nullable columns) and exits without serving requests.
   V0088 has a five-second lock timeout; if it times out, resolve the blocking
   transaction and rerun this step rather than deploying the new API.
2. Run `crates/database/src/migrations/out_of_band/refresh_token_rotation_index.sql`
   outside a transaction. Its final query must return `true`.
3. Switch traffic to the new API code, then deploy the UI. If deployment uses
   more than one API process, avoid serving refresh requests from old and new
   versions at the same time: old code cannot accept a predecessor token after
   new code rotates it. Monitor refresh 401/5xx rates during rollout.

The columns and index can remain if code is rolled back. Roll back UI before
API; the old API can still read the current token hash, but loses the grace
behavior.
