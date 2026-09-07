CREATE TABLE organization_member_role_audit_log (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id),
    member_user_id UUID NOT NULL REFERENCES users(id),
    changed_by_user_id UUID NOT NULL REFERENCES users(id),
    previous_role VARCHAR(20) NOT NULL CHECK (previous_role IN ('admin', 'member')),
    new_role VARCHAR(20) NOT NULL CHECK (new_role IN ('admin', 'member')),
    changed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_org_member_role_audit_org_changed_at
ON organization_member_role_audit_log (organization_id, changed_at DESC);

CREATE INDEX idx_org_member_role_audit_member_changed_at
ON organization_member_role_audit_log (member_user_id, changed_at DESC);

CREATE INDEX idx_org_member_role_audit_actor_changed_at
ON organization_member_role_audit_log (changed_by_user_id, changed_at DESC);
