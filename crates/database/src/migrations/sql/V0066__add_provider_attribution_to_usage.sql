ALTER TABLE organization_usage_log
    ADD COLUMN IF NOT EXISTS served_provider_tier TEXT;

ALTER TABLE organization_usage_log
    ADD COLUMN IF NOT EXISTS served_provider_type TEXT;

ALTER TABLE organization_usage_log
    ADD COLUMN IF NOT EXISTS served_via_fallback BOOLEAN NOT NULL DEFAULT false;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'chk_org_usage_served_provider_tier'
          AND conrelid = 'organization_usage_log'::regclass
    ) THEN
        ALTER TABLE organization_usage_log
            ADD CONSTRAINT chk_org_usage_served_provider_tier
            CHECK (
                served_provider_tier IS NULL
                OR served_provider_tier IN ('near', 'attested_3p', 'non_attested')
            ) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'chk_org_usage_served_provider_type'
          AND conrelid = 'organization_usage_log'::regclass
    ) THEN
        ALTER TABLE organization_usage_log
            ADD CONSTRAINT chk_org_usage_served_provider_type
            CHECK (
                served_provider_type IS NULL
                OR served_provider_type IN ('vllm', 'external', 'chutes')
            ) NOT VALID;
    END IF;
END $$;

COMMENT ON COLUMN organization_usage_log.served_provider_tier IS
    'Actual provider tier that served the request: near, attested_3p, or non_attested.';

COMMENT ON COLUMN organization_usage_log.served_provider_type IS
    'Actual provider implementation that served the request: vllm, external, chutes, or future checked values.';

COMMENT ON COLUMN organization_usage_log.served_via_fallback IS
    'True when the request was served by a fallback provider after an earlier provider failed.';
