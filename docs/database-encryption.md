# Database encryption key rotation

`DB_ENCRYPTION_KEYS` is an ordered comma-separated list of `id=source` entries.
The first entry supplies the write key; every entry can decrypt existing
field envelopes. Use a distinct ID and distinct key bytes for each key. Keep
each ID bound to the same bytes on every replica throughout the rotation.

Sources are `dstack:<derivation-path>` or `file:<path>` containing a trimmed
64-digit hex key. Key files should be readable only by the service user.
`hex:<64 hex digits>` is for **local development and tests only** and is
rejected at configuration load unless `DB_ENCRYPTION_ALLOW_INLINE_KEYS=true`
is explicitly set. Key sources use the SDK default endpoint, the same endpoint
as attestation signing keys.

When the ring is configured, legacy `DB_ENCRYPTION_KEY`,
`DB_ENCRYPTION_KEY_FILE`, and `DB_ENCRYPTION_KEY_ID` are ignored. Include the
legacy key as a read entry if existing envelopes still need it. With
`DB_ENCRYPTION_KEYS` unset, legacy configuration behavior is unchanged.
Object-storage encryption keys are separate from database field keys.

1. Deploy ring-aware code to every replica, then configure both old and new
   keys with the old key first.
2. Put the new key first on every replica. A replica still on the previous
   configuration continues writing old-key envelopes. Compare
   `GET /v1/admin/database-encryption/keys` write IDs and fingerprints across
   replicas before starting rewrap.
3. Submit explicitly field-scoped `POST /v1/admin/database-encryption/jobs`
   requests with `actions: ["rewrap"]`. Review `mode: "dry_run"`, then use
   `mode: "execute"` with `DB_ENCRYPTION_WRITE_ENABLED=true`.
4. Use `POST /v1/admin/database-encryption/verify` to catch old-key stragglers
   and invalid or plaintext rows. Remove the previous key only after all
   relevant scopes verify successfully; verify again with the reduced ring.

Rewrap preserves plaintext, current-key and invalid envelopes. Per-key counts
include authenticated envelopes under configured ring IDs only; unknown or
malformed key IDs contribute only to invalid-envelope counters.

`batch_delay_ms` accepts 0–10000 and throttles every mode, including verify and
dry-run scans. Set it at the top level or in `scope.options`, never both.
Between batches the transaction has committed and the pooled connection is
returned before sleeping. Cancellation is checked before and after sleep
slices of at most 500 ms. The process worker permit remains held so only one
job runs per process at a time.

See [env.example](../env.example) for configuration and request examples.
