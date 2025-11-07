-- Refactor files table to track by API key instead of user
-- Change uploaded_by_user_id to uploaded_by_api_key_id

-- Step 1: Add new column as nullable first
ALTER TABLE files ADD COLUMN uploaded_by_api_key_id UUID;

-- Step 2: Populate uploaded_by_api_key_id from existing data
-- Find an API key that was created by the user in the same workspace
UPDATE files f
SET uploaded_by_api_key_id = (
    SELECT ak.id
    FROM api_keys ak
    WHERE ak.created_by_user_id = f.uploaded_by_user_id
    AND ak.workspace_id = f.workspace_id
    AND ak.is_active = true
    ORDER BY ak.created_at DESC
    LIMIT 1
)
WHERE f.uploaded_by_user_id IS NOT NULL;

-- Step 3: Delete any files that couldn't be mapped to an API key
-- This handles orphaned files where the user or API keys no longer exist
DELETE FROM files WHERE uploaded_by_api_key_id IS NULL;

-- Step 4: Make the column NOT NULL
ALTER TABLE files ALTER COLUMN uploaded_by_api_key_id SET NOT NULL;

-- Step 5: Add foreign key constraint with ON DELETE CASCADE
ALTER TABLE files ADD CONSTRAINT fk_files_api_key
    FOREIGN KEY (uploaded_by_api_key_id)
    REFERENCES api_keys(id)
    ON DELETE CASCADE;

-- Step 6: Drop the old column
ALTER TABLE files DROP COLUMN uploaded_by_user_id;
