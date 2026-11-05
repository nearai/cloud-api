# Object encryption key rotation

S3 objects use AES-256-GCM with the existing `nonce || ciphertext || tag` format. Object key identifiers live in S3 user metadata named `near-enc-key-id` (`x-amz-meta-near-enc-key-id` over HTTP). The envelope bytes do not change.

## Configuration

`S3_ENCRYPTION_KEYS` is an ordered, comma-separated list of `id=source` entries. The first entry writes new objects; the remaining entries only read existing objects. Sources use the same grammar as `DB_ENCRYPTION_KEYS`:

```text
S3_ENCRYPTION_KEYS=s3-v2=dstack:/nearai-cloud-api/object-encryption/v2,s3-v1=file:/config/object-encryption-key-v1
```

Sources can be `file:/absolute/path` (a hex-encoded key), `dstack:/key/path` (a derived key), or `hex:<64 hex characters>` for local development and tests only. Inline sources are rejected unless `S3_ENCRYPTION_ALLOW_INLINE_KEYS=true` is explicitly set; the database inline-key switch does not enable object inline keys. Every key must contain exactly 32 bytes. Invalid sources or keys stop startup without printing key material.

When `S3_ENCRYPTION_KEYS` is unset, `S3_ENCRYPTION_KEY_FILE` or `S3_ENCRYPTION_KEY` supplies the single key, with implicit ID `s3-legacy`. With the ring configured, the legacy single-key variables are ignored without reading their values or files; startup emits one warning if either remains set. Include the legacy key as a read entry when existing objects still require it. Use distinct IDs and distinct key bytes; the ring rejects duplicate IDs or key material and zeroes its keys on drop. Configure the same ordered keys on every replica before starting a rotation.

`OBJECT_REWRAP_MAX_BYTES` is a positive integer byte limit, default `268435456` (256 MiB). Objects whose HEAD `Content-Length` exceeds it are counted as `oversized` without downloading their bodies. This bounds rewrap and verification memory; uploads are unaffected.

`OBJECT_ENCRYPTION_WRITE_ENABLED` is the object rewrap kill switch, default `true`. It accepts only `true` or `false`. Setting it to `false` refuses new execute jobs with HTTP 400 and stops existing execute jobs when the worker next checks the gate. Dry runs, scans, verification, and uploads remain available; uploads continue using the configured write key. `/keys` reports the actual gate value as `write_enabled`.

A known metadata ID selects only that key. Normal object reads try the ring for missing or unknown metadata. Scan and verify classify an unconfigured metadata ID as `undecryptable` with class `unknown_key_id`, without trial decryption. Execute tries the ring to recover such objects and emits at most one warning per job with the unknown-ID count. Missing metadata can be resolved by authentication against the ring. Keep previous keys available until all objects have been rewrapped and verified.

## Administrator endpoints

All endpoints are under `/v1/admin/object-encryption` and use the existing administrator authentication. Sessions and read-write administrator tokens can start jobs. Read-only tokens can inspect keys/jobs and run scans. Job creation, cancellation, and verification require write permission; insufficient permission returns `403 insufficient_permissions`.

Only one queued or running job may have the same stored scope, including its options. Creating another through `/jobs` or `/verify` returns HTTP 409 with `an object encryption job is already queued or running`. Jobs with different options have distinct scopes; production bucket-wide leases still serialize their storage work. A terminal job does not block a new job with the same scope.

| Method | Path | Result |
|---|---|---|
| GET | `/keys` | Write key ID, ordered key IDs, fingerprints, and `write_enabled`; no key material |
| POST | `/scan` | Bounded counts of current bucket objects, a `complete` flag, and a continuation cursor |
| POST | `/jobs` | Creates a resumable dry-run or execute job |
| GET | `/jobs/{id}` | Job status, progress, cursor, and redacted error class |
| POST | `/jobs/{id}/cancel` | Requests cancellation between objects or during the inter-batch delay |
| POST | `/verify` | Creates a resumable verification job |

Both object and database `/keys` fingerprints are the first 16 lowercase hexadecimal characters of `HMAC-SHA256(key, "near-key-fingerprint/v1")`. Compare IDs and fingerprints across replicas; the endpoint never returns key material. Job responses omit bucket names. Each sample-key list is capped at 20 entries, while total counters cover every processed object.

Inspect the bucket before rewriting objects:

```http
POST /v1/admin/object-encryption/scan
Content-Type: application/json

{"limit":10000,"deadline_seconds":60}
```

`limit` defaults to 10000 and must be 1–100000; larger values return HTTP 400. `deadline_seconds` is optional, defaults to 60, and must be positive and at most 60. A scan truncated by either bound returns `complete: false` and a cursor. Continue by passing that returned object unchanged as `cursor` in the next scan request, with the desired limit and deadline. The opaque cursor contains `continuation`, `last_key`, and `complete`; a partial page can also include `pending_keys` and `next_continuation`. It contains no bucket name. The response's counters describe that scan call, so combine counts across calls for a full scan.

Scan metadata counts group IDs outside the ring under `unknown` and missing metadata under `none`; arbitrary unconfigured IDs never become map keys. Scans do not prove ciphertext authentication. Use verification for that check.

Run a dry run, followed by an execute job with the same options:

```json
{
  "mode": "dry_run",
  "actions": ["rewrap"],
  "batch_size": 100,
  "batch_delay_ms": 100
}
```

Object jobs always cover the whole configured bucket. `scope` accepts only `options`; table, field, prefix, or bucket selectors are rejected. `batch_delay_ms` accepts 0–10000 in every job mode, including dry run and verify. Specify it at the top level or in `scope.options`, never both; duplicates return HTTP 400 even when equal. The transaction and pooled connection are released before inter-batch sleep. Cancellation is checked between objects and before and after sleep slices of at most 500 ms; an in-progress object operation must finish or time out first. The process worker permit remains held so only one object job runs per process at a time.

Change `mode` to `execute` to re-encrypt readable previous-key objects under the write key. Progress includes `rewrapped`, `already_current`, `stale`, `undecryptable`, `missing`, `changed`, `errors`, and `oversized`. An object deleted between listing and HEAD, GET, or PUT is `missing` and advances the cursor. A conditional PUT that loses a race (HTTP 412 / `PreconditionFailed`) is `changed` and leaves the current object untouched. Run execute again when `changed > 0` to revisit these objects.

Transient storage failures and per-call timeouts are retried with a short backoff. Exhausted retries and other per-object storage errors are counted under `errors` with a sample key and error class; they do not stop the remaining objects. Objects unavailable because of their storage class also count as `errors`. Undecryptable objects are counted without stopping the job. Job-level failures, including database errors, an execute gate turned off, or invalid configuration, fail the job. Repeating execute is safe: objects already under the write key are not rewritten.

Rewrites preserve content type and user metadata, replacing the encryption key ID with the write ID. They do not preserve `Cache-Control`, `Content-Disposition`, `Content-Encoding`, tags, ACL, or storage class. This matches the metadata that uploads set today; review externally managed object properties before rewrap.

Jobs use the existing job table with `scope.kind = "objects"`; database-field workers ignore these rows and object workers ignore database-field rows. Storage I/O runs outside database transactions and advisory or row locks. Short transactions claim or renew worker ownership and persist cursor and progress. Progress is committed after every completed object, independently of `batch_size` (more frequently than the 50-object or 10-second checkpoint requirement). A UUID-fenced lease in cursor JSON expires after 120 seconds and is renewed every 5 seconds while storage I/O or batch delays are in progress. The advisory transaction lock is used only during the short claim transaction; storage calls hold no database connection or lock. Workers resume from persisted progress after a restart; a replacement before its progress commit is safe to retry because rewrap is idempotent.

Verify with:

```http
POST /v1/admin/object-encryption/verify
Content-Type: application/json

{"batch_size":100,"batch_delay_ms":100}
```

Verification requires write-key metadata and successful authentication under the write key alone. It passes only when `stale == 0`, `undecryptable == 0`, and `errors == 0`, and no oversized object remains outside the write key ID. `oversized` is reported separately: a current metadata ID passes the size check without downloading that body, so its ciphertext is not authenticated during that run. `missing` and `changed` do not fail verification.

## Rotation sequence

1. Deploy ring-aware code to every replica, then configure old and new keys with the old key first.
2. Put the new key first on every replica and compare `/keys` write IDs and fingerprints.
3. Run dry-run and execute jobs for the whole bucket. A replica or an in-flight job still using the old configuration during a rolling deployment can leave stale objects. After the rollout completes and old writers finish, run execute again, then verify. Repeat execute for `changed` objects and resolve errors, stale objects, undecryptable objects, and oversized objects outside the write key ID before retiring a key.
4. After passing verification with previous keys present, deploy the write-only ring to every replica and verify again before discarding old key material.

## Concurrent deletion during rewrap

Rewrap uses the GET response's ETag for a conditional `If-Match` PUT when available. If a replacement or deletion invalidates that precondition, the object is counted as `changed`. If storage does not supply an ETag, the PUT is unconditional: a file deleted during its own rewrap may be recreated. Its file row is gone, so the object is an orphan that the next listing-based cleanup can remove. A PUT reporting `NoSuchKey` or an object-not-found HTTP 404 counts as `missing`; missing buckets, permission failures, and other per-object storage failures count as `errors`.

## Versioned buckets

Rewrap replaces the current version only. Deleting superseded versions in a versioned bucket, setting lifecycle rules, and removing old versions from replica buckets are operator steps outside this application. Retain access to previous keys for any object versions and backups that must remain readable.
