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
--
-- ASYMMETRY vs. the seed path (intentional): on the seed path, the
-- over-advertising risk of `tools` (sglang tool-calling is model-family specific:
-- it needs a compatible chat template + tool-call parser) is bounded by the
-- inactive-by-default gate plus an operator warning. This backfill has no such
-- gate — a row that is already `is_active = true` starts advertising `tools` the
-- moment this migration runs. The blast radius is external routers only (the data
-- plane never gates request handling on `supported_features`), and the prior
-- empty-empty state was itself wrong, so this is a strict improvement. Still,
-- at deploy time verify tool support on any active Chutes model this touches, and
-- clear `supported_features` via `PATCH /v1/admin/models` for any family that
-- lacks it.

-- Single statement so the set of rows actually mutated is captured exactly via
-- RETURNING, then audited in model_history — matching the app write path, which
-- the raw UPDATE would otherwise bypass (V0051 deliberately mirrored both
-- capability columns into model_history, and the app always bumps updated_at and
-- writes a history snapshot on every model edit).
WITH backfilled AS (
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
        supported_features = ARRAY['tools', 'json_mode']::TEXT[],
        -- The app write path always bumps updated_at; mirror that so the change
        -- is not invisible (cf. crates/database/src/repositories/model.rs).
        updated_at = NOW()
    WHERE provider_type = 'chutes'
      AND supported_sampling_parameters = '{}'::TEXT[]
      AND supported_features = '{}'::TEXT[]
    RETURNING *
),
-- Step 1: close the currently-open history record (if any) for each row we just
-- backfilled, exactly as the app does before inserting a new snapshot.
closed AS (
    UPDATE model_history mh
    SET effective_until = NOW()
    FROM backfilled b
    WHERE mh.model_id = b.id
      AND mh.effective_until IS NULL
    RETURNING mh.model_id
)
-- Step 2: insert a new snapshot reflecting the post-update model state for every
-- backfilled row, with a change_reason that identifies this migration. We select
-- from `backfilled` (its columns already hold the post-UPDATE values) so the
-- snapshot is faithful regardless of whether an open prior record existed.
INSERT INTO model_history (
    model_id,
    input_cost_per_token,
    output_cost_per_token,
    cost_per_image,
    cache_read_cost_per_token,
    context_length,
    model_name,
    model_display_name,
    model_description,
    model_icon,
    verifiable,
    is_active,
    owned_by,
    provider_type,
    provider_config,
    attestation_supported,
    input_modalities,
    output_modalities,
    inference_url,
    hugging_face_id,
    quantization,
    max_output_length,
    supported_sampling_parameters,
    supported_features,
    datacenters,
    is_ready,
    deprecation_date,
    openrouter_slug,
    effective_from,
    effective_until,
    change_reason,
    created_at
)
SELECT
    b.id,
    b.input_cost_per_token,
    b.output_cost_per_token,
    b.cost_per_image,
    b.cache_read_cost_per_token,
    b.context_length,
    b.model_name,
    b.model_display_name,
    b.model_description,
    b.model_icon,
    b.verifiable,
    b.is_active,
    b.owned_by,
    b.provider_type,
    b.provider_config,
    b.attestation_supported,
    b.input_modalities,
    b.output_modalities,
    b.inference_url,
    b.hugging_face_id,
    b.quantization,
    b.max_output_length,
    b.supported_sampling_parameters,
    b.supported_features,
    b.datacenters,
    b.is_ready,
    b.deprecation_date,
    b.openrouter_slug,
    NOW(),
    NULL,
    'V0060 backfill: seed non-empty Chutes capabilities (#781 M1)',
    NOW()
FROM backfilled b;
