-- Add fields for conversation management features: pin, archive, delete, and clone tracking
-- Note: name/title is stored in metadata JSON field following OpenAI spec
-- Using timestamp fields instead of booleans for better audit trails and flexibility

-- Add pinned_at timestamp for pinning conversations
ALTER TABLE conversations ADD COLUMN pinned_at TIMESTAMPTZ DEFAULT NULL;

-- Add archived_at timestamp for archiving conversations
ALTER TABLE conversations ADD COLUMN archived_at TIMESTAMPTZ DEFAULT NULL;

-- Add deleted_at timestamp for soft deleting conversations
ALTER TABLE conversations ADD COLUMN deleted_at TIMESTAMPTZ DEFAULT NULL;

-- Add cloned_from_id to track if a conversation was cloned from another
ALTER TABLE conversations ADD COLUMN cloned_from_id UUID REFERENCES conversations(id) ON DELETE SET NULL;

-- Add indices for common queries (partial indexes for better performance)
CREATE INDEX idx_conversations_pinned ON conversations(workspace_id, pinned_at) WHERE pinned_at IS NOT NULL;
CREATE INDEX idx_conversations_archived ON conversations(workspace_id, archived_at) WHERE archived_at IS NOT NULL;
CREATE INDEX idx_conversations_deleted ON conversations(workspace_id, deleted_at) WHERE deleted_at IS NULL;
CREATE INDEX idx_conversations_cloned_from ON conversations(cloned_from_id);

-- Add comments explaining the timestamp-based state tracking
COMMENT ON COLUMN conversations.pinned_at IS 'Timestamp when the conversation was pinned. NULL means not pinned.';
COMMENT ON COLUMN conversations.archived_at IS 'Timestamp when the conversation was archived. NULL means not archived.';
COMMENT ON COLUMN conversations.deleted_at IS 'Timestamp when the conversation was soft-deleted. NULL means not deleted.';
COMMENT ON COLUMN conversations.cloned_from_id IS 'ID of the conversation this was cloned from. NULL means original conversation.';

