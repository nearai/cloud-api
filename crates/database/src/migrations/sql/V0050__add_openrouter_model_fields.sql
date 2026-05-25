-- Add OpenRouter-compatible model metadata columns.
--
-- These mirror the fields OpenRouter's provider spec requires
-- (https://openrouter.ai/docs/guides/community/for-providers) so that
-- `GET /v1/models` can expose them without inventing data at the route layer.
--
-- All columns are nullable / default empty; existing rows keep working and
-- admin endpoints can backfill values per-model.

-- HuggingFace identifier (e.g. "Qwen/Qwen3-VL-30B-A3B-Instruct"). Required by
-- OpenRouter only when the model is hosted on HuggingFace; NULL otherwise.
ALTER TABLE models ADD COLUMN hugging_face_id TEXT;

-- Quantization label. OpenRouter expects one of:
--   int4, int8, fp4, fp6, fp8, fp16, bf16, fp32
-- We don't enforce a CHECK constraint here so we can adopt new labels without
-- a migration; validation lives at the admin write path.
ALTER TABLE models ADD COLUMN quantization TEXT;

-- Maximum number of output tokens the model can produce in a single response.
-- Distinct from context_length, which is the combined input+output budget.
ALTER TABLE models ADD COLUMN max_output_length INTEGER;

-- Sampling parameters accepted by the model. OpenRouter's vocabulary:
--   temperature, top_p, top_k, min_p, top_a, frequency_penalty,
--   presence_penalty, repetition_penalty, stop, seed, max_tokens, logit_bias
ALTER TABLE models
    ADD COLUMN supported_sampling_parameters TEXT[] NOT NULL DEFAULT '{}';

-- Feature capabilities. OpenRouter's vocabulary:
--   tools, json_mode, structured_outputs, logprobs, web_search, reasoning
ALTER TABLE models
    ADD COLUMN supported_features TEXT[] NOT NULL DEFAULT '{}';

-- Mirror the new columns onto model_history so admin edits are audited the
-- same way as the existing fields.
ALTER TABLE model_history ADD COLUMN hugging_face_id TEXT;
ALTER TABLE model_history ADD COLUMN quantization TEXT;
ALTER TABLE model_history ADD COLUMN max_output_length INTEGER;
ALTER TABLE model_history
    ADD COLUMN supported_sampling_parameters TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE model_history
    ADD COLUMN supported_features TEXT[] NOT NULL DEFAULT '{}';
