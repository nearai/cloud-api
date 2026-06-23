WITH invalid_active_models AS (
    SELECT *
    FROM models
    WHERE is_active
      AND COALESCE(max_output_length, 0) <= 0
),
closed_history AS (
    UPDATE model_history mh
    SET effective_until = NOW()
    FROM invalid_active_models invalid
    WHERE mh.model_id = invalid.id
      AND mh.effective_until IS NULL
    RETURNING mh.model_id
),
deactivated_models AS (
    UPDATE models m
    SET is_active = FALSE,
        updated_at = NOW()
    FROM invalid_active_models invalid
    WHERE m.id = invalid.id
    RETURNING m.*
)
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
    allow_free,
    effective_from,
    effective_until,
    change_reason,
    created_at
)
SELECT
    model.id,
    model.input_cost_per_token,
    model.output_cost_per_token,
    model.cost_per_image,
    model.cache_read_cost_per_token,
    model.context_length,
    model.model_name,
    model.model_display_name,
    model.model_description,
    model.model_icon,
    model.verifiable,
    model.is_active,
    model.owned_by,
    model.provider_type,
    model.provider_config,
    model.attestation_supported,
    model.input_modalities,
    model.output_modalities,
    model.inference_url,
    model.hugging_face_id,
    model.quantization,
    model.max_output_length,
    COALESCE(model.supported_sampling_parameters, ARRAY[]::TEXT[]),
    COALESCE(model.supported_features, ARRAY[]::TEXT[]),
    model.datacenters,
    model.is_ready,
    model.deprecation_date,
    model.openrouter_slug,
    model.allow_free,
    NOW(),
    NULL,
    'V0067: deactivate active model missing positive max_output_length',
    NOW()
FROM deactivated_models model;

ALTER TABLE models
    ADD CONSTRAINT chk_active_models_have_positive_max_output_length
    CHECK (NOT is_active OR (max_output_length IS NOT NULL AND max_output_length > 0));
