-- Raise the advertised max output of z-ai/glm-5.3-flash from 131,072 to
-- 1,048,576 tokens (the model's full context window).
--
-- Why: `max_output_length` is what `GET /v1/models` publishes as
-- `max_output_length` / `top_provider.max_completion_tokens`, and OpenRouter
-- reads it as our endpoint's max completion tokens. In default load balancing
-- OpenRouter only routes a request to providers whose max completion tokens
-- cover the request's `max_tokens`. Probed on 2026-09-29 with only NEAR AI and
-- Parasail (943,718) allowed: `max_tokens` 1,000 routed to us 9/12 times,
-- `max_tokens` 500,000 routed to us 0/12 times, and failed with 429 rather than
-- fall back to us when Parasail was saturated. Many clients set `max_tokens` to
-- the model maximum even for short answers, so 131,072 excludes us from that
-- traffic. Open Inference and ~half of the other OpenRouter providers for this
-- model list 943,718-1,179,648.
--
-- Safety: OpenRouter itself rejects prompt + `max_tokens` above the listed
-- context (1,048,576) before dispatch, so the engine never sees an
-- over-context request from OpenRouter. The engine has no separate output cap
-- (SGLang `--context-length 1048576`); a lab run of the production long-tier
-- engine completed a forced 943,718-token generation with no errors or
-- retractions.
--
-- Scope: only the glm-5.3-flash row, and only while it still holds the old
-- 131,072 value, so an operator's manual edit via `PATCH /v1/admin/models` is
-- never overwritten and the migration is a no-op in environments without the
-- row. Audited in model_history exactly like the app write path (close the open
-- snapshot, insert a new one), following V0060.
WITH raised AS (
    UPDATE models
    SET
        max_output_length = 1048576,
        updated_at = NOW()
    WHERE model_name = 'z-ai/glm-5.3-flash'
      AND max_output_length = 131072
    RETURNING *
),
closed AS (
    UPDATE model_history mh
    SET effective_until = NOW()
    FROM raised r
    WHERE mh.model_id = r.id
      AND mh.effective_until IS NULL
    RETURNING mh.model_id
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
    created_at,
    text_pricing
)
SELECT
    r.id,
    r.input_cost_per_token,
    r.output_cost_per_token,
    r.cost_per_image,
    r.cache_read_cost_per_token,
    r.context_length,
    r.model_name,
    r.model_display_name,
    r.model_description,
    r.model_icon,
    r.verifiable,
    r.is_active,
    r.owned_by,
    r.provider_type,
    r.provider_config,
    r.attestation_supported,
    r.input_modalities,
    r.output_modalities,
    r.inference_url,
    r.hugging_face_id,
    r.quantization,
    r.max_output_length,
    r.supported_sampling_parameters,
    r.supported_features,
    r.datacenters,
    r.is_ready,
    r.deprecation_date,
    r.openrouter_slug,
    r.allow_free,
    NOW(),
    NULL,
    'V0083: raise glm-5.3-flash max_output_length 131072 -> 1048576 for OpenRouter max_tokens routing',
    NOW(),
    r.text_pricing
FROM raised r;
