-- Test databases need nullable `organization_id UUID` and `workspace_id UUID` columns on
-- `chat_signatures`. Adds them when the pinned schema does not already contain them.
ALTER TABLE chat_signatures ADD COLUMN IF NOT EXISTS organization_id UUID;
ALTER TABLE chat_signatures ADD COLUMN IF NOT EXISTS workspace_id UUID;
