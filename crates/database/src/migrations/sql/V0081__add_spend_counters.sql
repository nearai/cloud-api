-- Persist split inference/service spend totals for bounded analytics reads.
-- All values are integer nano-dollars (scale 9, USD).
-- Existing rows are initialized to zero; an out-of-band backfill must run before
-- these columns are used as lifetime totals.

ALTER TABLE organization_balance
    ADD COLUMN inference_spent BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN service_spent BIGINT NOT NULL DEFAULT 0;

CREATE TABLE api_key_spend (
    api_key_id UUID PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
    inference_spent BIGINT NOT NULL DEFAULT 0,
    service_spent BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

COMMENT ON TABLE api_key_spend IS 'Cached per-API-key inference and service spending totals in nano-dollars';
COMMENT ON COLUMN api_key_spend.inference_spent IS 'Cumulative inference spending in nano-dollars (scale 9, USD)';
COMMENT ON COLUMN api_key_spend.service_spent IS 'Cumulative service spending in nano-dollars (scale 9, USD)';
