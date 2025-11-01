-- V8: Add model aliases table
-- This migration adds support for model name aliases
-- Allows clients to use friendly/alternative names that resolve to canonical vLLM model names

-- Model aliases table
-- Maps alias names (client-facing) to canonical model names (vLLM official names)
-- Example: "phala/gpt-oss-120b" (alias) -> "openai/gpt-oss-120b" (canonical)
-- Example: "deepseek/deepseek-v3.1" (alias) -> "deepseek-ai/DeepSeek-V3.1" (canonical)
CREATE TABLE model_aliases (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    alias_name VARCHAR(500) NOT NULL UNIQUE, -- Client-facing alias name
    canonical_model_id UUID NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Add indexes for efficient lookups
CREATE INDEX idx_model_aliases_alias ON model_aliases(alias_name);
CREATE INDEX idx_model_aliases_canonical ON model_aliases(canonical_model_id);
CREATE INDEX idx_model_aliases_active ON model_aliases(is_active);

-- Create a trigger to update the updated_at timestamp
CREATE OR REPLACE FUNCTION update_model_aliases_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trigger_update_model_aliases_updated_at
    BEFORE UPDATE ON model_aliases
    FOR EACH ROW
    EXECUTE FUNCTION update_model_aliases_updated_at();

