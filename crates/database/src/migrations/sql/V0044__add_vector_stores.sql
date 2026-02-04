-- Thin workspace-to-vector-store ref for auth + pagination
-- id is the RAG service's UUID (reused, not auto-generated)
CREATE TABLE vector_stores (
    id           UUID PRIMARY KEY,  -- RAG service's UUID, provided on insert
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at   TIMESTAMPTZ
);
CREATE INDEX idx_vs_workspace ON vector_stores(workspace_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_vs_pagination ON vector_stores(workspace_id, created_at DESC, id)
    WHERE deleted_at IS NULL;
