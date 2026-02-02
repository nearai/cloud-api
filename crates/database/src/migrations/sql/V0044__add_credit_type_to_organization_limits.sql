-- V44: Add credit type, source, and currency to organization limits history
-- This migration adds support for tracking different types of credits (grant vs payment)
-- with optional source tracking and multi-currency support.

-- Add new columns to organization_limits_history
ALTER TABLE organization_limits_history 
    ADD COLUMN credit_type VARCHAR(50) NOT NULL DEFAULT 'payment',
    ADD COLUMN source VARCHAR(100),
    ADD COLUMN currency VARCHAR(10) NOT NULL DEFAULT 'USD';

-- Drop the old partial index for active limits (single active per org)
DROP INDEX IF EXISTS idx_org_limits_history_active;

-- Create new unique partial index scoped by credit_type
-- This ensures only one active limit per organization per credit type
CREATE UNIQUE INDEX idx_org_limits_active_by_type 
    ON organization_limits_history(organization_id, credit_type) 
    WHERE effective_until IS NULL;

-- Add index for querying by credit type
CREATE INDEX idx_org_limits_history_credit_type 
    ON organization_limits_history(organization_id, credit_type, effective_from DESC);

-- Add comment for clarity
COMMENT ON COLUMN organization_limits_history.credit_type IS 'Type of credit: grant (free credits) or payment (purchased credits)';
COMMENT ON COLUMN organization_limits_history.source IS 'Source of the credit: nearai, stripe, hot-pay, etc.';
COMMENT ON COLUMN organization_limits_history.currency IS 'Currency of the credit amount: USD, USDT, etc.';

