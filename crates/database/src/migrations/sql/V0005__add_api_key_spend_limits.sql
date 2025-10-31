-- Add spend limit to API keys table
-- All amounts use fixed scale of 9 (nano-dollars) and USD currency
ALTER TABLE api_keys ADD COLUMN spend_limit BIGINT;

-- Add index for querying API keys by spend limit
CREATE INDEX idx_api_keys_spend_limit ON api_keys(spend_limit) WHERE spend_limit IS NOT NULL;

-- Add comment explaining the spend limit field
COMMENT ON COLUMN api_keys.spend_limit IS 'Optional spending limit for this API key in nano-dollars (scale 9). NULL means no limit.';

