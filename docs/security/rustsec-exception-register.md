# RustSec Exception Register

This register is the human review record for RustSec advisories intentionally ignored by `cargo audit` and `cargo deny`.

Tracker: `nearai/infra#188`

## Review Ownership

- Owner: Inference team placeholder
- Review cadence: TODO
- Review trigger: dependency update, new affected code path, or RustSec advisory change

## Active Exceptions

| Advisory | Package | Type | Reason | Upstream/Tracking |
| --- | --- | --- | --- | --- |
| `RUSTSEC-2023-0071` | `rsa` | Vulnerability | The Marvin timing side-channel advisory does not affect current cloud-api JWT handling because cloud-api uses HS256/HMAC, not RSA algorithms. | Revisit before introducing RSA signing or verification. |
| `RUSTSEC-2024-0436` | `paste` | Unmaintained | Transitive dependency from `dstack-sdk` through Alloy. This is informational and not a direct runtime security issue. | Waiting for upstream Alloy/dstack dependency migration. |
| `RUSTSEC-2024-0388` | `derivative` | Unmaintained | Transitive dependency from `dstack-sdk` through Alloy/ruint/ark-ff. This is informational and not a direct runtime security issue. | Waiting for upstream Alloy/dstack dependency migration. |
| `RUSTSEC-2026-0173` | `proc-macro-error2` | Unmaintained | Build-time transitive dependency from `dstack-sdk` through Alloy proc-macro dependencies. This is informational and not a runtime dependency. | Existing tracking reference: `nearai/cloud-api#732`. |

## Maintenance Rules

- Add a row before adding a new advisory ignore to `.cargo/audit.toml` or `deny.toml`.
- Remove a row in the same PR that removes the corresponding ignore.
- Keep the `Reason` field specific to cloud-api usage rather than copying the advisory summary.
