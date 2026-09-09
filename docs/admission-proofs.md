# Commons admission proofs

`POST /v1/auth/admission-proof` exchanges a valid Cloud session access token for a short-lived, audience-bound assertion, accepting `audience`, `nonce` and `device_public_key`; the nonce and Ed25519 device key use canonical unpadded base64url encoding of 32 bytes. Issuance requires an active user with provider `near`, `github` or `google`, and a verified `sid` bound to a live session regardless of the global legacy-token compatibility setting.

The response contains `assertion` and `expires_at` (Unix seconds). The signed claims include issuer, audience, a pseudonymous subject, issuance and expiry times, a unique assertion ID, provider, nonce and device key. The lifetime is 120 seconds. An assertion confers no Cloud API authority and establishes no inference payment, staking balance or contribution consent.

`GET /v1/auth/admission-proof/jwks` publishes the Ed25519 verification keys without authentication. Both routes return 503 when issuance is unconfigured. Issuance responses carry `Cache-Control: no-store`; the public key response can be cached for 30 seconds.

## Configuration

| Setting | Value |
| --- | --- |
| `ADMISSION_PROOF_ISSUER` | Canonical HTTPS issuer identifier agreed with the verifier |
| `ADMISSION_PROOF_AUDIENCES` | Comma-separated exact audience identifiers |
| `ADMISSION_PROOF_SUBJECT_KEY_FILE` | File containing an independently generated random 32-byte key, encoded as unpadded base64url |
| `ADMISSION_PROOF_SIGNING_SEED_FILE` | File containing an independently generated random 32-byte Ed25519 seed, encoded as unpadded base64url |
| `ADMISSION_PROOF_RETAINED_PUBLIC_KEYS` | Optional comma-separated additional verification keys, each encoded as unpadded base64url |

Secret-file settings also have direct environment alternatives without `_FILE`. File settings take precedence. Restrict secret files to the service account. Keep both secrets independent of `AUTH_ENCODING_KEY`; never publish the session signing secret. Partial or invalid proof configuration prevents authentication initialization and startup.

Preserve the subject key across signing-key rotations; changing it changes contributor identities and requires a coordinated identity migration. Rotate the signer in two deployments:

1. Keep the current signing seed and add the incoming public key to `ADMISSION_PROOF_RETAINED_PUBLIC_KEYS` on every instance. Verify that every instance publishes both keys. Wait for previously cached key sets to expire, including any longer cache period configured by consumers.
2. Activate the new signing seed and retain the outgoing public key. Both deployments now publish both verification keys throughout the rolling update. After the last old signer stops, keep its public key available for at least the assertion lifetime plus the consumer's cache period and permitted clock skew before removing it.

## Consumer requirements

Configure the trusted issuer, audience and JWKS URL independently of the token. Pin `alg=EdDSA` and `typ=tc-admission+jwt`. Verify signature, key ID, time bounds and the expected challenge/device claims. Never follow a key URL supplied in a token. When a key ID is unknown, a consumer using remote JWKS may refresh once from its pinned endpoint before rejecting; coalesce concurrent refreshes and rate-limit them so attacker-selected key IDs cannot create unbounded fetches. Consumers with operator-provisioned key sets must receive the incoming key before the signer changes.

The Commons verifier must consume its enrollment challenge atomically and verify device possession before creating an account session. Cloud issuance does not consume the Commons challenge; repeated issuance for a challenge remains safe only when the consumer enforces one-time enrollment. Existing receipt and contribution checks still apply.

Real PostgreSQL tests exercise successful issuance, public-key verification, invalid-token rejection and logout revocation. Crypto tests cover changed claims, separate users/audiences and stable subjects across signer rotation. A deployed Commons verifier and a real browser wallet ceremony require their own integration validation.
