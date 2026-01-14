-- Add external provider support to models table
-- provider_type: 'vllm' (default, TEE-enabled) or 'external' (3rd party providers)
-- provider_config: JSON configuration for external providers
-- attestation_supported: whether the model supports TEE attestation

ALTER TABLE models ADD COLUMN provider_type VARCHAR(50) NOT NULL DEFAULT 'vllm';
ALTER TABLE models ADD COLUMN provider_config JSONB;
ALTER TABLE models ADD COLUMN attestation_supported BOOLEAN NOT NULL DEFAULT true;

-- Add to model_history for audit trail
ALTER TABLE model_history ADD COLUMN provider_type VARCHAR(50);
ALTER TABLE model_history ADD COLUMN provider_config JSONB;
ALTER TABLE model_history ADD COLUMN attestation_supported BOOLEAN;

-- Add index for querying external models
CREATE INDEX idx_models_provider_type ON models(provider_type);

-- Add comment explaining provider_config format
COMMENT ON COLUMN models.provider_config IS 'JSON config for external providers. Examples:
{"backend": "openai_compatible", "base_url": "https://api.openai.com/v1"}
{"backend": "anthropic", "base_url": "https://api.anthropic.com", "version": "2023-06-01"}
{"backend": "gemini", "base_url": "https://generativelanguage.googleapis.com"}';
