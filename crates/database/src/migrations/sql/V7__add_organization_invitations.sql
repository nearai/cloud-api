-- Create organization invitations table for managing pending invitations
CREATE TABLE organization_invitations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    email VARCHAR(255) NOT NULL,
    role VARCHAR(50) NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    invited_by_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status VARCHAR(50) NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'accepted', 'declined', 'expired')),
    token VARCHAR(255) UNIQUE NOT NULL, -- Unique token for accepting invitation via email link
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    responded_at TIMESTAMPTZ,
    
    -- Ensure we don't have duplicate pending invitations for the same email+org
    UNIQUE(organization_id, email, status)
);

-- Indexes for efficient queries
CREATE INDEX idx_org_invitations_org_id ON organization_invitations(organization_id);
CREATE INDEX idx_org_invitations_email ON organization_invitations(email);
CREATE INDEX idx_org_invitations_status ON organization_invitations(status);
CREATE INDEX idx_org_invitations_token ON organization_invitations(token);
CREATE INDEX idx_org_invitations_expires_at ON organization_invitations(expires_at) WHERE status = 'pending';

-- Comments
COMMENT ON TABLE organization_invitations IS 'Stores pending and historical invitations to organizations';
COMMENT ON COLUMN organization_invitations.token IS 'Unique token for accepting invitation via email link';
COMMENT ON COLUMN organization_invitations.status IS 'Status of invitation: pending, accepted, declined, or expired';

