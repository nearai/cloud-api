-- Add two more OpenRouter-compatible model metadata columns.
--
-- These mirror the provider spec
-- (https://openrouter.ai/docs/guides/community/for-providers) so that
-- `GET /v1/models` can expose them without inventing data at the route layer.
-- Follows the V0051 precedent: nullable columns, no CHECK constraints
-- (validation lives at the admin write path), and mirrored onto model_history
-- so admin edits are audited.

-- Whether the model is "ready". Per OpenRouter's spec, `false` keeps the model
-- hidden on their side and `true` enables auto-staging. Cloud API stores and
-- exposes this verbatim and does NOT change its own listing/filtering on it.
-- Nullable: NULL means "unset" and is omitted from the API response.
ALTER TABLE models ADD COLUMN is_ready BOOLEAN;

-- Planned deprecation date for the model. Stored as TIMESTAMPTZ normalized to
-- a whole UTC hour per the OpenRouter spec: a date-only input ("2026-01-01")
-- defaults to 13:00 UTC, and an explicit instant is truncated to the top of
-- its UTC hour ("2026-01-01T15:00:00Z"). Serialized back in the UTC-hour form
-- YYYY-MM-DDTHH:00:00Z. Nullable: NULL means "no planned deprecation".
ALTER TABLE models ADD COLUMN deprecation_date TIMESTAMPTZ;

-- Mirror the new columns onto model_history so admin edits are audited the
-- same way as the existing fields.
ALTER TABLE model_history ADD COLUMN is_ready BOOLEAN;
ALTER TABLE model_history ADD COLUMN deprecation_date TIMESTAMPTZ;
