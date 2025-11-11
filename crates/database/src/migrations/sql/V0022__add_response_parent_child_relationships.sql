-- Add child_response_ids column to responses table
-- This enables tracking child responses (responses created as follow-ups)
-- Note: parent relationship is already tracked via existing previous_response_id column

-- Add child_response_ids column as JSONB array
ALTER TABLE responses ADD COLUMN IF NOT EXISTS child_response_ids JSONB DEFAULT '[]'::jsonb;

-- Create GIN index on child_response_ids for efficient JSONB array queries
CREATE INDEX IF NOT EXISTS idx_responses_children ON responses USING GIN (child_response_ids);

