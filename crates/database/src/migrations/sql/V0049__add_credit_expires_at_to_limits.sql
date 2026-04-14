ALTER TABLE organization_limits_history 
    ADD COLUMN credit_expires_at TIMESTAMPTZ;

COMMENT ON COLUMN organization_limits_history.credit_expires_at IS 
    'When set, this credit grant expires at this time and is excluded from available credits';