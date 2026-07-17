CREATE TABLE organization_reporting_tokens (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL CHECK (length(trim(name)) > 0),
    token_hash VARCHAR(64) NOT NULL UNIQUE,
    token_prefix VARCHAR(16) NOT NULL,
    created_by_user_id UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    revoked_by_user_id UUID REFERENCES users(id)
);

CREATE INDEX idx_org_reporting_tokens_hash
    ON organization_reporting_tokens(token_hash);

CREATE INDEX idx_org_reporting_tokens_active_org
    ON organization_reporting_tokens(organization_id, created_at DESC, id DESC)
    WHERE revoked_at IS NULL;

CREATE INDEX idx_org_reporting_tokens_expiry
    ON organization_reporting_tokens(expires_at)
    WHERE revoked_at IS NULL AND expires_at IS NOT NULL;
