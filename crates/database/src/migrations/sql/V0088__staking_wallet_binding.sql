-- Refuse ambiguous legacy data instead of silently replacing accounting sources.
CREATE UNIQUE INDEX organization_staking_farm_sources_one_org_wallet
    ON organization_staking_farm_sources (organization_id, network_id, contract_id);

CREATE TABLE staking_wallet_binding_challenges (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id),
    actor_user_id UUID NOT NULL REFERENCES users(id),
    challenge JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    verification_attempts INTEGER NOT NULL DEFAULT 0,
    idempotency_key UUID,
    proof_digest TEXT,
    public_key TEXT,
    wallet_user_id UUID REFERENCES users(id),
    previous_role TEXT,
    result JSONB,
    UNIQUE (organization_id, actor_user_id, idempotency_key)
);
CREATE INDEX staking_wallet_binding_actor_created ON staking_wallet_binding_challenges(actor_user_id, created_at);
