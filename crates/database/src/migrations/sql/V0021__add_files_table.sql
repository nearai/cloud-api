-- Add files table for file uploads
CREATE TABLE files (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    filename VARCHAR(500) NOT NULL,
    bytes BIGINT NOT NULL,
    content_type VARCHAR(100) NOT NULL,
    purpose VARCHAR(20) NOT NULL,
    storage_key TEXT NOT NULL,
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    uploaded_by_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    CONSTRAINT files_purpose_check CHECK (purpose IN ('assistants', 'batch', 'fine-tune', 'vision', 'user_data', 'evals'))
);

-- Create indexes for common queries
CREATE INDEX idx_files_workspace_id ON files(workspace_id);
CREATE INDEX idx_files_created_at ON files(created_at);
CREATE INDEX idx_files_expires_at ON files(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX idx_files_purpose ON files(purpose);
