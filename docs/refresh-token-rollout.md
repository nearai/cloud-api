# Refresh-token grace rollout

Deploy in this order; do not start the new API code before the index is ready.
Before step 1, prepare and verify a rollback API image that contains the V0088
migration file but retains the previous refresh behavior. An image built before
V0088 cannot start after this migration is recorded: Refinery's default
`abort_missing` setting rejects unknown applied versions. The existing
`Rollback Image` workflow can select such an old image, so do not use its
automatic previous-tag selection for this rollout.

1. While the old API still serves traffic, run the candidate image with the
   same database configuration as production but invoke `./api --migrate-only`.
   It applies V0088 (two nullable columns) and exits without serving requests.
   V0088 has a five-second lock timeout; if it times out, resolve the blocking
   transaction and rerun this step rather than deploying the new API.
2. Run `crates/database/src/migrations/out_of_band/refresh_token_rotation_index.sql`
   outside a transaction. The script must exit successfully; if a failed
   concurrent build left an invalid index, drop that index and rerun the
   script. The new API also checks index validity at startup; `--migrate-only`
   does not require it.
3. Switch traffic to the new API code, then deploy the UI. If deployment uses
   more than one API process, avoid serving refresh requests from old and new
   versions at the same time: old code cannot accept a predecessor token after
   new code rotates it. Monitor refresh 401/5xx rates during rollout.

The successor token is derived from `auth.encoding_key`. Keep that key the same
across API instances during this rollout. Rotating it while a predecessor is
inside the 60-second reuse window can turn an otherwise recoverable retry into
a 401; coordinate key rotation separately.

The columns and index can remain if code is rolled back. Roll back UI before
API, and roll API back only to the verified V0088-compatible image. It can
still read the current token hash, but loses the grace behavior. If no such
image is available, pause the rollout before step 1 rather than relying on an
older image to restart.
