-- V0037: Add new columns to organization_usage_log and make request_type nullable
-- This migration adds:
-- 1. provider_request_id (raw ID from inference provider before hashing)
-- 2. stop_reason (why the stream ended)
-- 3. Makes request_type nullable (deprecated, replaced by inference_type)

-- Add provider_request_id column (raw request ID from vLLM before hashing to inference_id)
ALTER TABLE organization_usage_log
    ADD COLUMN IF NOT EXISTS provider_request_id VARCHAR(255);

-- Add stop_reason column for tracking how the stream ended
-- Values: completed, length, content_filter, client_disconnect, provider_error, timeout
ALTER TABLE organization_usage_log
    ADD COLUMN IF NOT EXISTS stop_reason VARCHAR(50);

-- Add index for provider_request_id lookups
CREATE INDEX IF NOT EXISTS idx_org_usage_provider_request_id
    ON organization_usage_log(provider_request_id)
    WHERE provider_request_id IS NOT NULL;

-- Update comments for documentation
COMMENT ON COLUMN organization_usage_log.provider_request_id IS
    'Raw request ID from the inference provider (e.g., vLLM chat_id) before hashing to inference_id.';

COMMENT ON COLUMN organization_usage_log.stop_reason IS
    'Why the inference stream ended: completed, length, content_filter, client_disconnect, provider_error, timeout.';

-- Make request_type nullable (deprecated, we now use inference_type instead)
-- This allows new code to stop writing to request_type while old records still have values
ALTER TABLE organization_usage_log
    ALTER COLUMN request_type DROP NOT NULL;

-- Backfill inference_type from request_type for existing records
-- This preserves historical data before eventual removal of request_type column
UPDATE organization_usage_log
SET inference_type = request_type
WHERE inference_type IS NULL AND request_type IS NOT NULL;
