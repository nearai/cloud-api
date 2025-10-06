-- V4: Add organization usage tracking tables
-- This migration adds comprehensive usage tracking for organizations to enforce credit limits

-- Organization usage log - detailed record of each API call's cost
-- This table stores every API call with its token usage and calculated costs
-- All costs use fixed scale of 9 (nano-dollars) and USD currency
CREATE TABLE organization_usage_log (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    api_key_id UUID NOT NULL REFERENCES api_keys(id),
    response_id UUID REFERENCES responses(id) ON DELETE SET NULL,
    
    -- Model and token usage
    model_id VARCHAR(255) NOT NULL,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0,
    
    -- Cost calculation (fixed scale 9 = nano-dollars, USD only)
    -- Example: $0.001 USD = 1,000,000 nano-dollars
    input_cost BIGINT NOT NULL,
    output_cost BIGINT NOT NULL,
    total_cost BIGINT NOT NULL,
    
    -- Metadata
    request_type VARCHAR(50) NOT NULL, -- 'chat_completion', 'text_completion', 'response'
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for efficient queries
CREATE INDEX idx_org_usage_organization ON organization_usage_log(organization_id);
CREATE INDEX idx_org_usage_workspace ON organization_usage_log(workspace_id);
CREATE INDEX idx_org_usage_api_key ON organization_usage_log(api_key_id);
CREATE INDEX idx_org_usage_created ON organization_usage_log(created_at);
CREATE INDEX idx_org_usage_response ON organization_usage_log(response_id) WHERE response_id IS NOT NULL;
CREATE INDEX idx_org_usage_model ON organization_usage_log(model_id);
CREATE INDEX idx_org_usage_org_created ON organization_usage_log(organization_id, created_at DESC);

-- Organization balance summary - cached aggregates for fast queries
-- This table provides O(1) lookup of current organization spending
-- All costs use fixed scale of 9 (nano-dollars) and USD currency
CREATE TABLE organization_balance (
    organization_id UUID PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    
    -- Current spending total (fixed scale 9 = nano-dollars, USD only)
    total_spent BIGINT NOT NULL DEFAULT 0,
    
    -- Aggregate statistics for analytics
    last_usage_at TIMESTAMPTZ,
    total_requests BIGINT NOT NULL DEFAULT 0,
    total_tokens BIGINT NOT NULL DEFAULT 0,
    
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_org_balance_updated ON organization_balance(updated_at);

-- Trigger function to automatically create balance record when organization is created
CREATE OR REPLACE FUNCTION create_organization_balance()
RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO organization_balance (
        organization_id,
        total_spent,
        updated_at
    ) VALUES (
        NEW.id,
        0,
        NOW()
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Attach trigger to organizations table
CREATE TRIGGER trigger_create_organization_balance
    AFTER INSERT ON organizations
    FOR EACH ROW
    EXECUTE FUNCTION create_organization_balance();

-- Backfill existing organizations with zero balance
INSERT INTO organization_balance (organization_id, total_spent, updated_at)
SELECT id, 0, NOW()
FROM organizations
WHERE id NOT IN (SELECT organization_id FROM organization_balance)
ON CONFLICT (organization_id) DO NOTHING;

-- Add comments for documentation
COMMENT ON TABLE organization_usage_log IS 'Detailed log of every API call with token usage and costs (all in nano-dollars, scale 9, USD)';
COMMENT ON TABLE organization_balance IS 'Cached aggregate spending totals for fast credit checks (all in nano-dollars, scale 9, USD)';
COMMENT ON COLUMN organization_usage_log.total_cost IS 'Total cost in nano-dollars (scale 9, USD)';
COMMENT ON COLUMN organization_balance.total_spent IS 'Cumulative spending in nano-dollars (scale 9, USD)';

