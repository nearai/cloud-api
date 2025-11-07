-- Response items table for storing individual response output items
-- This enables granular storage of messages, tool calls, reasoning, etc.
CREATE TABLE response_items (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    response_id UUID NOT NULL REFERENCES responses(id) ON DELETE CASCADE,
    conversation_id UUID REFERENCES conversations(id) ON DELETE CASCADE,
    api_key_id UUID NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    item JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for efficient querying
CREATE INDEX idx_response_items_response ON response_items(response_id);
CREATE INDEX idx_response_items_api_key ON response_items(api_key_id);
CREATE INDEX idx_response_items_conversation ON response_items(conversation_id);
CREATE INDEX idx_response_items_created ON response_items(created_at);

-- Trigger for updated_at
CREATE TRIGGER update_response_items_updated_at 
    BEFORE UPDATE ON response_items
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Remove input_messages and output_message columns from responses table
-- These should be stored as response_items instead
ALTER TABLE responses DROP COLUMN IF EXISTS input_messages;
ALTER TABLE responses DROP COLUMN IF EXISTS output_message;

-- Add workspace_id and api_key_id to conversations and responses tables
-- This changes the scope from user-based to workspace-based (developer platform model)

-- Safety check: Only delete data if migration hasn't been applied yet
-- This prevents data loss if migration is reapplied due to a bug
DO $$
BEGIN
    -- Check if migration has already been applied by checking if new columns exist
    IF EXISTS (
        SELECT 1 FROM information_schema.columns 
        WHERE table_name = 'conversations' AND column_name = 'workspace_id'
    ) THEN
        -- Migration already applied, skip DELETE to prevent data loss
        RAISE NOTICE 'Migration V0020 already applied - skipping data deletion';
    ELSE
        -- Migration not applied yet, safe to delete data
        DELETE FROM responses;
        DELETE FROM conversations;
    END IF;
END $$;

-- Drop user_id and add workspace_id/api_key_id to conversations table
ALTER TABLE conversations DROP COLUMN IF EXISTS user_id;
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS workspace_id UUID REFERENCES workspaces(id) ON DELETE CASCADE;
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS api_key_id UUID REFERENCES api_keys(id) ON DELETE CASCADE;

-- Now make them NOT NULL (safe since table is empty)
ALTER TABLE conversations ALTER COLUMN workspace_id SET NOT NULL;
ALTER TABLE conversations ALTER COLUMN api_key_id SET NOT NULL;

-- Drop user_id and add workspace_id/api_key_id to responses table
ALTER TABLE responses DROP COLUMN IF EXISTS user_id;
ALTER TABLE responses ADD COLUMN IF NOT EXISTS workspace_id UUID REFERENCES workspaces(id) ON DELETE CASCADE;
ALTER TABLE responses ADD COLUMN IF NOT EXISTS api_key_id UUID REFERENCES api_keys(id) ON DELETE CASCADE;

-- Now make them NOT NULL (safe since table is empty)
ALTER TABLE responses ALTER COLUMN workspace_id SET NOT NULL;
ALTER TABLE responses ALTER COLUMN api_key_id SET NOT NULL;

-- Create indexes
CREATE INDEX IF NOT EXISTS idx_conversations_workspace ON conversations(workspace_id);
CREATE INDEX IF NOT EXISTS idx_conversations_api_key ON conversations(api_key_id);
CREATE INDEX IF NOT EXISTS idx_responses_workspace ON responses(workspace_id);
CREATE INDEX IF NOT EXISTS idx_responses_api_key ON responses(api_key_id);

-- Drop old user_id indexes if they exist
DROP INDEX IF EXISTS idx_conversations_user;
DROP INDEX IF EXISTS idx_responses_user;

