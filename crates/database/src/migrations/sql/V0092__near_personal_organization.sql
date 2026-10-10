-- Shared provisioning for first NEAR login and wallet binding. The user row lock
-- serializes both paths, and the caller's transaction includes all provisioning.
CREATE FUNCTION ensure_near_personal_organization(wallet_user_id UUID) RETURNS UUID
LANGUAGE plpgsql AS $$
DECLARE
    personal_org UUID;
    account_name TEXT;
    membership_time TIMESTAMPTZ;
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
    INSERT INTO organizations(id, name)
      VALUES(personal_org, left(account_name, 210) || '-org-' || personal_org::text);
    INSERT INTO organization_members(organization_id, user_id, role, joined_at)
      VALUES(personal_org, wallet_user_id, 'owner', membership_time);
    INSERT INTO workspaces(name, organization_id, created_by_user_id)
      VALUES('default', personal_org, wallet_user_id);
    RETURN personal_org;
END;
$$;

-- Repair identities provisioned by the previous binding flow. Keep all existing
-- memberships and sources, and do not replace an existing owned organization.
SELECT ensure_near_personal_organization(u.id)
FROM users u
WHERE u.auth_provider = 'near' AND u.is_active
  AND EXISTS (SELECT 1 FROM staking_wallet_binding_challenges c
              WHERE c.wallet_user_id = u.id AND c.consumed_at IS NOT NULL);
