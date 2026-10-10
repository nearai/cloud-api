-- Refuse ambiguous legacy data instead of silently replacing accounting sources.
CREATE UNIQUE INDEX organization_staking_farm_sources_one_org_wallet
    ON organization_staking_farm_sources (organization_id, network_id, contract_id);
CREATE UNIQUE INDEX organization_staking_farm_sources_one_per_org
    ON organization_staking_farm_sources (organization_id);

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

CREATE INDEX staking_wallet_binding_org_wallet_consumed
    ON staking_wallet_binding_challenges (organization_id, wallet_user_id)
    WHERE consumed_at IS NOT NULL;

-- Shared provisioning for first NEAR login and wallet binding. The user row lock
-- serializes both paths, and the caller's transaction includes all provisioning.
CREATE FUNCTION ensure_near_personal_organization(wallet_user_id UUID) RETURNS UUID
LANGUAGE plpgsql AS $$
DECLARE
    personal_org UUID;
    account_name TEXT;
    membership_time TIMESTAMPTZ;
    generated_name TEXT;
    inserted_org UUID;
BEGIN
    SELECT provider_user_id INTO account_name FROM users
      WHERE id = wallet_user_id AND auth_provider = 'near' AND is_active FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'Active NEAR identity required';
    END IF;
    SELECT o.id INTO personal_org FROM organization_members m
      JOIN organizations o ON o.id = m.organization_id AND o.is_active
      WHERE m.user_id = wallet_user_id AND m.role = 'owner'
      ORDER BY m.joined_at, m.organization_id LIMIT 1;
    IF FOUND THEN RETURN personal_org; END IF;

    personal_org := uuid_generate_v4();
    -- Defaults are inferred from the first membership. Also repair identities
    -- invited before provisioning, and avoid equal NOW() values within one tx.
    SELECT LEAST(now(), COALESCE(min(joined_at), now())) - interval '1 microsecond'
      INTO membership_time FROM organization_members WHERE user_id = wallet_user_id;
    LOOP
        generated_name := left(account_name, 246) || '-org-' || left(uuid_generate_v4()::text, 4);
        INSERT INTO organizations(id, name) VALUES(personal_org, generated_name)
          ON CONFLICT(name) WHERE is_active = true DO NOTHING RETURNING id INTO inserted_org;
        EXIT WHEN inserted_org IS NOT NULL;
    END LOOP;
    INSERT INTO organization_members(organization_id, user_id, role, joined_at)
      VALUES(personal_org, wallet_user_id, 'owner', membership_time);
    INSERT INTO workspaces(name, organization_id, created_by_user_id)
      VALUES('default', personal_org, wallet_user_id);
    RETURN personal_org;
END;
$$;

