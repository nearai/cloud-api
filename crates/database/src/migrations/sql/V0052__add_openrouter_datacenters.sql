-- Add OpenRouter-compatible `datacenters` model metadata column.
--
-- OpenRouter's provider spec
-- (https://openrouter.ai/docs/guides/community/for-providers) lets a model
-- declare the datacenters it runs in as an array of objects with a
-- `country_code` field (ISO 3166 Alpha-2), e.g.
--   "datacenters": [{"country_code": "US"}]
--
-- We store only the country codes as a TEXT[] (the `[{country_code: ...}]`
-- object wrapper is reconstructed at the serialization layer). This mirrors
-- the array columns added in V0051 (supported_sampling_parameters,
-- supported_features), except `datacenters` is NULLable rather than
-- NOT NULL DEFAULT '{}': OpenRouter omits the field entirely when a provider
-- does not declare datacenters, and we want `GET /v1/models` to omit it too
-- (rather than emit an empty array) when unset. Existing rows keep working;
-- admin endpoints can backfill values per-model.
--
-- We don't enforce a CHECK constraint here so we can adopt codes without a
-- migration; validation (2-letter uppercase ISO Alpha-2) lives at the admin
-- write path, exactly as the other OpenRouter fields do.
ALTER TABLE models ADD COLUMN datacenters TEXT[];

-- Mirror the new column onto model_history so admin edits are audited the
-- same way as the existing fields.
ALTER TABLE model_history ADD COLUMN datacenters TEXT[];
