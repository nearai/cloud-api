-- V3: Add organization limits history table
-- This migration adds a table to track spending limit changes over time for organizations

-- Organization limits history table
-- Tracks spending limit changes over time with decimal precision (similar to model_pricing_history)
CREATE TABLE organization_limits_history (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    
    -- Spend limit using decimal representation (consistent with model pricing)
    -- Examples:
    --   $100.00 USD: amount=10000, scale=2, currency='USD'
    --   0.0001 BTC: amount=1, scale=4, currency='BTC'
    --   €50.00 EUR: amount=5000, scale=2, currency='EUR'
    spend_limit_amount BIGINT NOT NULL,        -- Amount in smallest unit
    spend_limit_scale INT NOT NULL DEFAULT 2,   -- Number of decimal places
    spend_limit_currency VARCHAR(10) NOT NULL DEFAULT 'USD', -- Currency code
    
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

