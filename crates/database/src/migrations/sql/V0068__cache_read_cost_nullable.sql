-- Make cache_read_cost_per_token nullable and stop overloading 0.
--
-- Before this migration the column was BIGINT NOT NULL DEFAULT 0, with 0
-- meaning "cache pricing disabled -> bill cached tokens at the full input
-- rate". That made a genuinely FREE cache-read price (0) inexpressible.
--
-- New semantics:
--   NULL = cache pricing disabled (cached tokens billed at input_cost_per_token,
--          input_cache_read omitted from the public model catalog)
--   >= 0 = cached tokens billed at this rate (0 = genuinely free)
--
-- Existing 0 rows meant "disabled", so they are converted to NULL: billing
-- for every existing model is unchanged.

ALTER TABLE models ALTER COLUMN cache_read_cost_per_token DROP NOT NULL;
ALTER TABLE models ALTER COLUMN cache_read_cost_per_token DROP DEFAULT;
UPDATE models SET cache_read_cost_per_token = NULL WHERE cache_read_cost_per_token = 0;

ALTER TABLE model_history ALTER COLUMN cache_read_cost_per_token DROP NOT NULL;
ALTER TABLE model_history ALTER COLUMN cache_read_cost_per_token DROP DEFAULT;
UPDATE model_history SET cache_read_cost_per_token = NULL WHERE cache_read_cost_per_token = 0;

-- scheduled_model_pricing_changes:
--   * old_cache_read_cost_per_token is the pricing snapshot at confirm time;
--     it mirrors models.cache_read_cost_per_token, so NULL = cache pricing
--     was disabled when the change was confirmed. Existing 0 snapshots meant
--     "disabled" and are converted the same way.
--   * new_cache_read_cost_per_token is deliberately left as-is: it is already
--     nullable and NULL there means "field unchanged" (see V0058), NOT
--     "disable cache pricing". A scheduled change therefore cannot disable
--     cache pricing; use PATCH /v1/admin/models with an explicit null for that.
ALTER TABLE scheduled_model_pricing_changes ALTER COLUMN old_cache_read_cost_per_token DROP NOT NULL;
UPDATE scheduled_model_pricing_changes SET old_cache_read_cost_per_token = NULL WHERE old_cache_read_cost_per_token = 0;
