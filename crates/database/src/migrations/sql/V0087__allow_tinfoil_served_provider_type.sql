-- FORWARD-ONLY. refinery runs with abort_missing=true (refinery-core 0.9.1
-- default): reverting the code that added this file must KEEP this file, or
-- startup fails with MissingVersion. Widening the CHECK is harmless to old code.
ALTER TABLE organization_usage_log
    DROP CONSTRAINT IF EXISTS chk_org_usage_served_provider_type;

ALTER TABLE organization_usage_log
    ADD CONSTRAINT chk_org_usage_served_provider_type
    CHECK (
        served_provider_type IS NULL
        OR served_provider_type IN ('vllm', 'external', 'chutes', 'tinfoil')
    ) NOT VALID;

COMMENT ON COLUMN organization_usage_log.served_provider_type IS
    'Actual provider implementation that served the request: vllm, external, chutes, tinfoil, or future checked values.';
