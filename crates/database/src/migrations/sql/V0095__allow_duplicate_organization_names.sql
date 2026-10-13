-- First-login and wallet-binding provisioning previously used ON CONFLICT(name).
-- Replace that runtime dependency before removing the name uniqueness index.
CREATE OR REPLACE FUNCTION ensure_near_personal_organization(wallet_user_id UUID) RETURNS UUID
LANGUAGE plpgsql AS $$
DECLARE
    personal_org UUID;
    account_name TEXT;
    membership_time TIMESTAMPTZ;
    generated_name TEXT;
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
    IF FOUND THEN
        -- Inactive workspaces retain their history. The active-only unique
        -- index lets us provision a replacement without reviving deleted data.
        INSERT INTO workspaces (name, organization_id, created_by_user_id)
            VALUES ('default', personal_org, wallet_user_id)
            ON CONFLICT (organization_id, name) WHERE is_active = true DO NOTHING;
        RETURN personal_org;
    END IF;

    personal_org := uuid_generate_v4();
    -- Defaults are inferred from the first membership. Also repair identities
    -- invited before provisioning, and avoid equal NOW() values within one tx.
    SELECT LEAST(now(), COALESCE(min(joined_at), now())) - interval '1 microsecond'
      INTO membership_time FROM organization_members WHERE user_id = wallet_user_id;
    -- Labels may repeat; the user row lock and UUID keep provisioning isolated.
    generated_name := left(account_name, 246) || '-org-' || left(uuid_generate_v4()::text, 4);
    INSERT INTO organizations(id, name) VALUES(personal_org, generated_name);
    INSERT INTO organization_members(organization_id, user_id, role, joined_at)
      VALUES(personal_org, wallet_user_id, 'owner', membership_time);
    INSERT INTO workspaces(name, organization_id, created_by_user_id)
      VALUES('default', personal_org, wallet_user_id);
    RETURN personal_org;
END;
$$;

-- Organization names are display labels, not tenant identifiers. Keep the UUID
-- primary key and all existing ID-based relationships unchanged.
DROP INDEX IF EXISTS unique_organization_name_active_only;

-- V0040 already removed this constraint. Also remove it on installations where
-- it was retained, so both active and inactive organizations may share a name.
ALTER TABLE organizations DROP CONSTRAINT IF EXISTS organizations_name_key;
