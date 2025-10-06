-- V3: Add organization limits history table
-- This migration adds a table to track spending limit changes over time for organizations

-- Organization limits history table
-- Tracks spending limit changes over time
-- All amounts use fixed scale of 9 (nano-dollars) and USD currency
CREATE TABLE organization_limits_history (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    
    -- Spend limit (fixed scale 9 = nano-dollars, USD only)
    -- Example: $100.00 USD = 100,000,000,000 nano-dollars
    spend_limit BIGINT NOT NULL,
    
    -- Temporal tracking for audit trail
    effective_from TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    effective_until TIMESTAMPTZ, -- NULL = currently active limit
    
    -- Change tracking
    changed_by VARCHAR(255),     -- e.g., "billing_service", user_id, admin_user_id
    change_reason VARCHAR(500),  -- e.g., "Customer purchased $100 credits"
    
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for efficient queries
CREATE INDEX idx_org_limits_history_org ON organization_limits_history(organization_id);
CREATE INDEX idx_org_limits_history_effective ON organization_limits_history(effective_from, effective_until);

-- Partial index for fast lookup of current active limits
CREATE INDEX idx_org_limits_history_active ON organization_limits_history(organization_id, effective_from DESC) 
    WHERE effective_until IS NULL;

-- Index for temporal queries (what was the limit at time X?)
CREATE INDEX idx_org_limits_history_temporal ON organization_limits_history(organization_id, effective_from, effective_until);

