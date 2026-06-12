-- Backfill OpenRouter capability metadata onto already-deployed Chutes rows.
--
-- Issue #781 (item M1): the Chutes catalog seed in
-- `crate::ensure_chutes_catalog_row` (api crate) historically inserted rows with
-- the V0051 empty-array defaults for `supported_sampling_parameters` /
-- `supported_features`. OpenRouter-style routers gate tool/function-calling on
-- those arrays, so an empty list advertises the model as supporting *nothing*
-- and routers silently refuse to route tool calls to it.
--
-- The companion code change seeds non-empty defaults on NEW rows, but
-- `seed_model_if_absent` is `INSERT ... ON CONFLICT DO NOTHING`, so it never
-- touches rows that already exist. The Chutes rows where the bug was observed
-- already exist with the empty-empty defaults, so without this backfill
-- `GET /v1/models` would keep advertising empty capabilities after deploy.
--
-- Scope is deliberately narrow and self-healing: only Chutes rows whose BOTH
-- capability arrays are still exactly the empty default. That empty-empty state
-- is precisely the bug — it was the SQL default, never an intentional operator
-- configuration (an operator setting capabilities via the admin API always
-- writes a non-empty array, and an operator who deliberately wanted an empty
-- list would only ever clear one of the two, not land on empty-empty by hand).
-- Any row an operator has already curated is left untouched.
--
-- Values mirror `CHUTES_SUPPORTED_SAMPLING_PARAMS` / `CHUTES_SUPPORTED_FEATURES`
-- in the api crate (and stay within `routes::admin::VALID_*` vocabulary, asserted
-- by the `chutes_seed_values_are_valid_openrouter_vocabulary` unit test). Keep
-- the two in sync if either side changes.
UPDATE models
SET
    supported_sampling_parameters = ARRAY[
        'temperature',
        'top_p',
        'frequency_penalty',
        'presence_penalty',
        'stop',
        'seed',
        'max_tokens'
    ]::TEXT[],
    supported_features = ARRAY[
        'tools',
        'json_mode'
    ]::TEXT[]
WHERE provider_type = 'chutes'
  AND supported_sampling_parameters = '{}'::TEXT[]
  AND supported_features = '{}'::TEXT[];
