-- Add next_response_ids column to responses table
-- This enables tracking next responses (responses created as follow-ups)
-- Note: previous relationship is already tracked via existing previous_response_id column

-- Add next_response_ids column as JSONB array
ALTER TABLE responses ADD COLUMN IF NOT EXISTS next_response_ids JSONB DEFAULT '[]'::jsonb;

-- Create GIN index on next_response_ids for efficient JSONB array queries
CREATE INDEX IF NOT EXISTS idx_responses_next ON responses USING GIN (next_response_ids);

