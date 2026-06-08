-- Add an OpenRouter-compatible `openrouter.slug` override column.
--
-- OpenRouter's provider spec
-- (https://openrouter.ai/docs/guides/community/for-providers) requires the
-- model `id` we expose in `GET /v1/models` to EXACTLY match OpenRouter's
-- canonical slug for that model. When our canonical `model_name` differs from
-- OpenRouter's slug (e.g. `zai-org/GLM-5.1-FP8` vs `z-ai/glm-5.1`), the spec
-- lets a provider supply an explicit override via a nested
--   "openrouter": { "slug": "<value>" }
-- object on the model. We store that override slug here.
--
-- Follows the V0051/V0052/V0053 precedent: a nullable column with no CHECK
-- constraint (validation — a lowercase `author/slug` shape — lives at the
-- admin write path, exactly as the other OpenRouter fields do), and mirrored
-- onto model_history so admin edits are audited the same way. NULL means
-- "unset": `GET /v1/models` omits the nested `openrouter` object entirely
-- rather than emitting an empty one.
ALTER TABLE models ADD COLUMN openrouter_slug TEXT;

-- Mirror the new column onto model_history so admin edits are audited the
-- same way as the existing fields.
ALTER TABLE model_history ADD COLUMN openrouter_slug TEXT;
