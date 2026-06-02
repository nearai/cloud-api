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

-- Planned deprecation date for the model. Stored as TIMESTAMPTZ so callers can
-- supply either a date ("2026-01-01") or an hour-precision instant
-- ("2026-01-01T00:00:00Z") per the OpenRouter spec; serialized back as an
-- ISO 8601 string. Nullable: NULL means "no planned deprecation".
ALTER TABLE models ADD COLUMN deprecation_date TIMESTAMPTZ;

-- Mirror the new columns onto model_history so admin edits are audited the
-- same way as the existing fields.
ALTER TABLE model_history ADD COLUMN is_ready BOOLEAN;
ALTER TABLE model_history ADD COLUMN deprecation_date TIMESTAMPTZ;
