-- Add cache read pricing and usage tracking
-- This migration adds:
-- 1. cache_read_cost_per_token to models and model_history (nano-dollars, 0 = no cache pricing)
-- 2. cache_read_tokens to organization_usage_log for tracking cached prompt tokens

-- Add cache_read_cost_per_token to models table (nano-dollars, scale 9)
-- Default 0 means no cache read pricing; when set, cached tokens are billed at this rate
ALTER TABLE models ADD COLUMN cache_read_cost_per_token BIGINT NOT NULL DEFAULT 0;

-- Add cache_read_cost_per_token to model_history for historical pricing tracking
ALTER TABLE model_history ADD COLUMN cache_read_cost_per_token BIGINT NOT NULL DEFAULT 0;

-- Add cache_read_tokens to usage log (subset of input_tokens that were cache hits)
ALTER TABLE organization_usage_log ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;
