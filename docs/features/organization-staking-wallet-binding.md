# Organization staking wallet binding

Implements [House of Stake #228](https://github.com/nearai/house-of-stake-contracts/issues/228).

This service no longer serves the bind route, the staking farm configuration route, the farm state read routes or the admin farm routes (`POST /v1/organizations/{org_id}/staking/farm/bind` and its siblings). `POST /v1/organizations/{org_id}/staking/farm/sync` and the `staking_organization_id` field of `/v1/users/me` remain, and existing bindings stay in effect. The description below records the binding design.

The bind route accepted a `phase` discriminator:

- `prepare`: `{ "phase": "prepare", "near_account_id": "alice.near" }` returns a five-minute, actor/org/wallet-scoped NEP-413 challenge. No identity, source or membership is created.
- `confirm`: send `challenge_id`, `signed_message` (camelCase NEP-413 wallet output), `acknowledged_terms_version`, and a UUID `idempotency_key`. The backend verifies the stored message and full-access public key, applies wallet AML policy and atomically commits the immutable source, verified NEAR Cloud identity, Admin membership, proof audit and retry result. It returns `source` and `wallet_membership`, without login tokens.

The signed terms explicitly approve permanent credit association, the non-deletable organization, and full organization Admin access. Existing Members become Admins; Admins remain Admins; Owners retain Owner access. Provider identity (`near`, account ID) is authoritative, not email. Disabled users are rejected. No synthetic email invitation or separate acceptance step is sent.

Binding ensures the active NEAR provider identity has an owned personal organization and its default workspace before granting Admin membership in the selected organization. First NEAR login uses the same atomic database provisioning, so concurrent binding/login cannot create duplicate personal organizations. Existing active owned organizations are preserved. V0092 provisions personal Owner memberships before invited memberships in the existing default ordering, using account-org plus a four-character random suffix with collision retries. Staking sources and their credit destinations are unchanged.

Membership grants happen only once in binding, never in login, sync, or idempotent retries. Later demotion/removal remains effective. Wallet ownership and direct contract rights remain independent of Cloud membership. `/users/me` supplies `staking_organization_id` only for an authenticated NEAR identity that remains a member of the active bound organization; clients use it as a selection preference, without changing default membership ordering.

Sync is owner/admin-only and uses the bound source's account, not the login wallet. Raw reward units and lifetime conversion remain authoritative; credited totals are monotonic even across concurrent replica updates. Read-only members do not receive wallet controls.

## Current state

Existing bindings stay in effect, and sources keep syncing regardless of member login provider. Sync returns `staking_farm_not_bound` for an organization without a bound source. The schema (V0092) enforces one source per organization and keeps the shared NEAR personal-organization provisioning with short names. The full-access NEP-413 proof path is the only supported wallet proof; Fireblocks and contract multisig are not supported by it.

Binding grants and source writes share the organization deletion/role-change lock. RPC sync happens separately after binding, so failure cannot undo the source or Admin membership. Retry confirmation with exactly the same proof and idempotency key after a lost response. Expired unconsumed challenges need a new prepare request. Challenge issuance is limited to ten per actor per ten minutes and proof verification to five attempts per challenge. Consumed challenge records retain the proof digest/key and previous/effective membership for audit; raw signatures are not persisted. Expired unconsumed challenges older than one day are pruned during the actor's next prepare; consumed audit records remain retained and indexed. Prepare rejects organizations with any existing source before signing, even across contract/network changes. Terms acknowledgment is compared with the version stored in the challenge.

For wallet-action preflight the frontend can reuse the existing sync endpoint, which checks the actual source wallet's policy, rather than adding an action-check API or treating the current user's status as treasury eligibility. A failed sync is a distinct state even when its HTTP request succeeded.

## Validation

- `cargo test -p services --lib staking_farm`: tampered/expired/mis-scoped proof, Admin terms, full-access vs function-call key verification, source-wallet AML, and farm regressions.
- `cargo test -p api --lib routes::staking_farm`: discriminated API request validation.
- `PGHOST=... PGPORT=... PGUSER=... PGDATABASE=... cargo test -p database --test staking_wallet_binding`: migrated PostgreSQL, atomic membership/source creation, existing roles, disabled identities, concurrent/conflicting binding, postpay and role-revocation races, retry without access restoration.

The bound NEAR provider identity has permanent organization membership: member removal and self-leave are rejected under the organization lock, including after unstaking. Other members can still be removed normally. Role changes do not unbind the staking source.
