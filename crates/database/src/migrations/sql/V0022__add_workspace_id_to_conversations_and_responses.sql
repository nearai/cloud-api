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
        RAISE NOTICE 'Migration V0022 already applied - skipping data deletion';
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

-- Update response_items table: drop user_id and add api_key_id
-- Safety check: Only delete data if migration hasn't been applied yet
DO $$
BEGIN
    -- Check if migration has already been applied by checking if new column exists
    IF EXISTS (
        SELECT 1 FROM information_schema.columns 
        WHERE table_name = 'response_items' AND column_name = 'api_key_id'
    ) THEN
        -- Migration already applied, skip DELETE to prevent data loss
        RAISE NOTICE 'Migration V0022 already applied - skipping response_items deletion';
    ELSE
        -- Migration not applied yet, safe to delete data
        DELETE FROM response_items;
    END IF;
END $$;

-- Drop user_id and add api_key_id to response_items table
ALTER TABLE response_items DROP COLUMN IF EXISTS user_id;
ALTER TABLE response_items ADD COLUMN IF NOT EXISTS api_key_id UUID REFERENCES api_keys(id) ON DELETE CASCADE;

-- Now make it NOT NULL (safe since table is empty)
ALTER TABLE response_items ALTER COLUMN api_key_id SET NOT NULL;

-- Create index for api_key_id
CREATE INDEX IF NOT EXISTS idx_response_items_api_key ON response_items(api_key_id);

-- Drop old user_id index if it exists
DROP INDEX IF EXISTS idx_response_items_user;

