# Organization staking wallet binding

Implements [House of Stake #228](https://github.com/nearai/house-of-stake-contracts/issues/228).

`POST /v1/organizations/{org_id}/staking/farm/bind` accepts a `phase` discriminator:

- `prepare`: `{ "phase": "prepare", "near_account_id": "alice.near" }` returns a five-minute, actor/org/wallet-scoped NEP-413 challenge. No identity, source or membership is created.
- `confirm`: send `challenge_id`, `signed_message` (camelCase NEP-413 wallet output), `acknowledged_terms_version`, and a UUID `idempotency_key`. The backend verifies the stored message and full-access public key, applies wallet AML policy and atomically commits the immutable source, verified NEAR Cloud identity, Admin membership, proof audit and retry result. It returns `source` and `wallet_membership`, without login tokens.

The signed terms explicitly approve permanent credit association, the non-deletable organization, and full organization Admin access. Existing Members become Admins; Admins remain Admins; Owners retain Owner access. Provider identity (`near`, account ID) is authoritative, not email. Disabled users are rejected. No synthetic email invitation or separate acceptance step is sent.

Membership grants happen only once in binding, never in login, sync, or idempotent retries. Later demotion/removal remains effective. Wallet ownership and direct contract rights remain independent of Cloud membership. `/users/me` supplies `staking_organization_id` only for an authenticated NEAR identity that remains a member of the active bound organization; clients use it as a selection preference, without changing default membership ordering.

GET farm state is member-readable and returns explicit `binding_status`, nullable `source`, and caller capabilities. For bound sources the old top-level farm fields remain available for older consumers. Sync is owner/admin-only and uses the bound source's account, not the login wallet. Raw reward units and lifetime conversion remain authoritative; credited totals are monotonic even across concurrent replica updates. Read-only members do not receive wallet controls.

## Rollout

1. Apply V0088. Its new org/network/contract unique index deliberately refuses ambiguous legacy rows; reconcile these before retrying migration. Do not delete financial history to bypass the constraint.
2. Deploy backend and matching nearai-cloud-ui. Leave `STAKING_FARM_SELECTED_ORG_BINDING_ENABLED=false` until both are ready. Existing sources remain usable and bound responses preserve old field names.
3. Enable the flag with staking enabled and configured. New source creation now requires explicit binding; unbound sync returns `staking_farm_not_bound`. Existing sources continue syncing regardless of member login provider.
4. Verify a full-access NEP-413 wallet in the deployed environment. Fireblocks/contract multisig are not claimed as supported by this proof path.

Binding grants and source writes share the organization deletion/role-change lock. RPC sync happens separately after binding, so failure cannot undo the source or Admin membership. Retry confirmation with exactly the same proof and idempotency key after a lost response. Expired unconsumed challenges need a new prepare request. Challenge issuance is limited to ten per actor per ten minutes and proof verification to five attempts per challenge. Consumed challenge records retain the proof digest/key and previous/effective membership for audit; raw signatures are not persisted.

For wallet-action preflight the frontend can reuse the existing sync endpoint, which checks the actual source wallet's policy, rather than adding an action-check API or treating the current user's status as treasury eligibility. A failed sync is a distinct state even when its HTTP request succeeded.

## Validation

- `cargo test -p services --lib staking_farm`: tampered/expired/mis-scoped proof, Admin terms, full-access vs function-call key verification, source-wallet AML, and farm regressions.
- `cargo test -p api --lib routes::staking_farm`: discriminated API request validation.
- `PGHOST=... PGPORT=... PGUSER=... PGDATABASE=... cargo test -p database --test staking_wallet_binding`: migrated PostgreSQL, atomic membership/source creation, existing roles, disabled identities, concurrent/conflicting binding, postpay and role-revocation races, retry without access restoration.
