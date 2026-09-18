-- Preserve the designation independently of membership/ownership and listing pagination.
ALTER TABLE users ADD COLUMN default_organization_id UUID REFERENCES organizations(id) ON DELETE RESTRICT;
ALTER TABLE users ADD COLUMN default_organization_source TEXT NOT NULL DEFAULT 'pending'
    CHECK (default_organization_source IN ('pending', 'first_membership', 'earliest_retained_membership', 'unresolved'));
CREATE INDEX idx_users_default_organization ON users(default_organization_id)
    WHERE default_organization_id IS NOT NULL;

-- Include inactive organizations. Memberships that were physically removed cannot be
-- reconstructed: expose the provenance rather than claiming this is the original.
WITH earliest AS (
    SELECT DISTINCT ON (user_id) user_id, organization_id
    FROM organization_members
    ORDER BY user_id, joined_at ASC, organization_id ASC
)
UPDATE users u SET default_organization_id = e.organization_id,
    default_organization_source = 'earliest_retained_membership'
FROM earliest e WHERE u.id = e.user_id;
-- A later invitation must not silently resolve a legacy user's missing history.
UPDATE users SET default_organization_source = 'unresolved'
WHERE default_organization_id IS NULL;

CREATE FUNCTION assign_first_default_organization() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE selected_org UUID;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM users WHERE id = NEW.user_id AND default_organization_source = 'pending') THEN
        RETURN NEW;
    END IF;
    -- Serialize with deletion/deactivation. A row lock on users serializes concurrent
    -- initial memberships; once assigned, the designation never changes.
    SELECT organization_id INTO selected_org FROM organization_members
    WHERE user_id = NEW.user_id ORDER BY joined_at ASC, organization_id ASC LIMIT 1;
    PERFORM 1 FROM organizations WHERE id = selected_org AND is_active FOR UPDATE;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;
    UPDATE users SET default_organization_id = selected_org,
        default_organization_source = 'first_membership'
    WHERE id = NEW.user_id AND default_organization_source = 'pending';
    RETURN NEW;
END;
$$;
CREATE TRIGGER assign_first_default_organization
AFTER INSERT ON organization_members FOR EACH ROW EXECUTE FUNCTION assign_first_default_organization();

CREATE FUNCTION protect_default_organization_designation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.default_organization_source <> 'pending' AND
       (NEW.default_organization_id IS DISTINCT FROM OLD.default_organization_id OR
        NEW.default_organization_source IS DISTINCT FROM OLD.default_organization_source) THEN
        RAISE EXCEPTION 'Default organization designation cannot be changed';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER protect_default_organization_designation
BEFORE UPDATE OF default_organization_id, default_organization_source ON users
FOR EACH ROW EXECUTE FUNCTION protect_default_organization_designation();

CREATE FUNCTION protect_default_organization() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (TG_OP = 'DELETE' OR NOT NEW.is_active) AND
       EXISTS (SELECT 1 FROM users WHERE default_organization_id = OLD.id) THEN
        RAISE EXCEPTION 'Default organizations cannot be deleted or deactivated';
    END IF;
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER protect_default_organization
BEFORE UPDATE OF is_active OR DELETE ON organizations
FOR EACH ROW EXECUTE FUNCTION protect_default_organization();
