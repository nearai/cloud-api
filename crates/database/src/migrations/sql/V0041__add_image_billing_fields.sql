-- Add image generation billing fields
-- This migration adds:
-- 1. cost_per_image to models table for per-image pricing
-- 2. cost_per_image to model_history table for historical tracking
-- 3. image_count to organization_usage_log for tracking image generation usage

-- Add cost_per_image to models table (nano-dollars, scale 9)
-- Default 0 means no cost for image generation (token-based models)
ALTER TABLE models ADD COLUMN cost_per_image BIGINT NOT NULL DEFAULT 0;

-- Add cost_per_image to model_history table for historical pricing tracking
ALTER TABLE model_history ADD COLUMN cost_per_image BIGINT NOT NULL DEFAULT 0;

-- Add image_count to usage log (nullable - only set for image generation requests)
ALTER TABLE organization_usage_log ADD COLUMN image_count INTEGER;

-- Add index for querying image generation usage
CREATE INDEX idx_usage_log_image_count ON organization_usage_log(image_count) WHERE image_count IS NOT NULL;
