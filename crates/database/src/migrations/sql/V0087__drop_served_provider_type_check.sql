-- served_provider_type values are owned by the application enum
-- (`ServedProviderType` in crates/services/src/usage/provider_attribution.rs).
-- The DB stores them unconstrained, so adding a provider never needs a
-- constraint migration. Readers tolerate unknown values.
--
-- FORWARD-ONLY. refinery runs with abort_missing=true (refinery-core 0.9.1
-- default): reverting the code that added this file must KEEP this file, or
-- startup fails with MissingVersion. Dropping the CHECK is harmless to old code.
ALTER TABLE organization_usage_log
    DROP CONSTRAINT IF EXISTS chk_org_usage_served_provider_type;

COMMENT ON COLUMN organization_usage_log.served_provider_type IS
    'Actual provider implementation that served the request (ServedProviderType::as_str). Unconstrained; owned by the application enum.';
