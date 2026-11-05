# Object encryption key rotation

S3 objects use AES-256-GCM with the existing `nonce || ciphertext || tag` format. Object key identifiers live in S3 user metadata named `near-enc-key-id` (`x-amz-meta-near-enc-key-id` over HTTP). The envelope bytes do not change.

## Configuration

`S3_ENCRYPTION_KEYS` is an ordered, comma-separated list of `id=source` entries. The first entry writes new objects; the remaining entries only read existing objects. Sources use the same grammar as `DB_ENCRYPTION_KEYS`:

```text
S3_ENCRYPTION_KEYS=s3-v2=dstack:/nearai-cloud-api/object-encryption/v2,s3-v1=file:/config/object-encryption-key-v1
```

Sources can be `file:/absolute/path` (a hex-encoded key), `dstack:/key/path` (a derived key), or `hex:<64 hex characters>` for local configuration. Every key must contain exactly 32 bytes. Invalid sources or keys stop startup without printing key material.

When `S3_ENCRYPTION_KEYS` is unset, `S3_ENCRYPTION_KEY_FILE` or `S3_ENCRYPTION_KEY` supplies the single key, with implicit ID `s3-legacy`. With the ring configured, the single-key variables can be omitted. Configure the same ordered keys on every replica before starting a rotation.

A known metadata ID selects only that key. Objects with missing or unknown metadata try the write key and then previous keys in order. AES-GCM authentication determines which key matches. Keep previous keys available until all objects have been rewrapped and verified.

## Administrator endpoints

All endpoints are under `/v1/admin/object-encryption` and use the existing administrator authentication. Sessions and read-write administrator tokens can start jobs. Read-only tokens can inspect keys/jobs and run scans. Job creation, cancellation, and verification require write permission; insufficient permission returns `403 insufficient_permissions`.

| Method | Path | Result |
|---|---|---|
| GET | `/keys` | Write key ID, ordered key IDs, and SHA-256 fingerprints; no keys |
| POST | `/scan` | Counts current bucket objects by metadata key ID; `none` means missing metadata |
| POST | `/jobs` | Creates a resumable dry-run or execute job |
| GET | `/jobs/{id}` | Job status, progress, cursor, and redacted error class |
| POST | `/jobs/{id}/cancel` | Requests cancellation at the next batch boundary |
| POST | `/verify` | Creates a resumable verification job |

For example, inspect the bucket before rewriting objects:

```http
POST /v1/admin/object-encryption/scan
Content-Type: application/json

{}
```

Then run a dry run, followed by an execute job with the same options:

```json
{
  "mode": "dry_run",
  "actions": ["rewrap"],
  "batch_size": 100,
  "batch_delay_ms": 100
}
```

Change `mode` to `execute` to re-encrypt objects under the write key. Each replacement preserves the content type and records the write key ID. Progress counts `rewrapped`, `already_current`, and `undecryptable`, with at most 100 sample object keys for the last category. Scans and all job modes also count `missing` objects, with up to 100 sample keys in `missing_keys`. An object deleted between listing and its metadata read, download, or rewrap is skipped, advances progress and the cursor, and does not prevent verification from passing. Other storage errors still fail the operation. An object that cannot be decrypted is counted and does not stop the job. A second execute job reports no rewrites for objects already marked current.

Jobs use the existing job table with `scope.kind = "objects"`; database-field workers ignore these rows and object workers ignore database-field rows. The continuation token, last processed key, and progress are committed together for each batch under a separate advisory lock. Restarted workers resume from the persisted cursor. An interruption after an object replacement but before its progress commit is safe to retry because rewrap is idempotent.

Verify with:

```http
POST /v1/admin/object-encryption/verify
Content-Type: application/json

{"batch_size":100,"batch_delay_ms":100}
```

Verification requires both write-key metadata and successful authentication under the write key alone. It passes only when `stale` and `undecryptable` are both zero. After a passing verification, deploy the write-only ring to all replicas and verify again before retiring previous keys.

## Concurrent deletion during rewrap

Rewrap uses the GET response's ETag for a conditional `If-Match` PUT when available, preventing a deleted object from being recreated by that write. If storage does not supply an ETag, the PUT is unconditional: a file deleted during its own rewrap may be recreated. Its file row is gone, so the object is an orphan that the next listing-based cleanup can remove. A PUT that reports `NoSuchKey` or an object-not-found HTTP 404 is counted as missing; permission errors, missing buckets, precondition failures, and other write conflicts still fail the job.

## Versioned buckets

Rewrap replaces the current version only. Deleting superseded versions in a versioned bucket, setting lifecycle rules, and removing old versions from replica buckets are operator steps outside this application. Retain access to previous keys for any object versions and backups that must remain readable.
