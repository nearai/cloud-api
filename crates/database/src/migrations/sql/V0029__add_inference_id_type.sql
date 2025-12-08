-- V0029: Add inference_type and inference_id (rolling deployment safe)
-- This migration adds new columns without dropping old ones for backwards compatibility
-- Phase 1: Add columns and write to both old/new during transition
-- Phase 2: (V0030) Backfill, make inference_type NOT NULL, and drop request_type

-- Add inference_type column (nullable during transition period)
ALTER TABLE organization_usage_log
    ADD COLUMN IF NOT EXISTS inference_type VARCHAR(50);

-- Add inference_id column
ALTER TABLE organization_usage_log
    ADD COLUMN IF NOT EXISTS inference_id UUID;

-- Add partial index for queries (only non-null values)
CREATE INDEX IF NOT EXISTS idx_org_usage_inference_id
    ON organization_usage_log(inference_id)
    WHERE inference_id IS NOT NULL;

-- Update comments for documentation
COMMENT ON COLUMN organization_usage_log.inference_id IS
    'Inference UUID for inference level usage tracking.';

COMMENT ON COLUMN organization_usage_log.inference_type IS
    'Type of inference request: chat_completion, chat_completion_stream, image_generation, embeddings, etc.';

COMMENT ON COLUMN organization_usage_log.request_type IS
    'DEPRECATED: Use inference_type instead. Kept for backwards compatibility during rolling deployment.';
