-- Vector Stores tables for OpenAI-compatible file search API

-- =============================================================================
-- Table: vector_stores
-- =============================================================================
CREATE TABLE vector_stores (
    id                          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id                UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name                        VARCHAR(512),
    description                 TEXT,
    status                      VARCHAR(50) NOT NULL DEFAULT 'completed',
    usage_bytes                 BIGINT NOT NULL DEFAULT 0,
    file_counts_in_progress     INTEGER NOT NULL DEFAULT 0,
    file_counts_completed       INTEGER NOT NULL DEFAULT 0,
    file_counts_failed          INTEGER NOT NULL DEFAULT 0,
    file_counts_cancelled       INTEGER NOT NULL DEFAULT 0,
    file_counts_total           INTEGER NOT NULL DEFAULT 0,
    last_active_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_after_anchor        VARCHAR(50),
    expires_after_days          INTEGER,
    expires_at                  TIMESTAMPTZ,
    metadata                    JSONB NOT NULL DEFAULT '{}',
    chunking_strategy           JSONB NOT NULL DEFAULT '{"type":"auto"}',
    created_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at                  TIMESTAMPTZ
);

-- Indexes for vector_stores
CREATE INDEX idx_vector_stores_workspace_id ON vector_stores(workspace_id);
CREATE INDEX idx_vector_stores_status ON vector_stores(status);
CREATE INDEX idx_vector_stores_created_at ON vector_stores(created_at);
CREATE INDEX idx_vector_stores_deleted_at ON vector_stores(deleted_at);

-- Check constraints for vector_stores
ALTER TABLE vector_stores ADD CONSTRAINT chk_vector_stores_status
    CHECK (status IN ('expired', 'in_progress', 'completed'));

ALTER TABLE vector_stores ADD CONSTRAINT chk_vector_stores_file_counts_non_negative
    CHECK (
        file_counts_in_progress >= 0 AND
        file_counts_completed >= 0 AND
        file_counts_failed >= 0 AND
        file_counts_cancelled >= 0 AND
        file_counts_total >= 0
    );

ALTER TABLE vector_stores ADD CONSTRAINT chk_vector_stores_usage_bytes_non_negative
    CHECK (usage_bytes >= 0);

-- Trigger for updated_at
CREATE TRIGGER set_vector_stores_updated_at
    BEFORE UPDATE ON vector_stores
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- =============================================================================
-- Table: vector_store_file_batches
-- =============================================================================
CREATE TABLE vector_store_file_batches (
    id                          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    vector_store_id             UUID NOT NULL REFERENCES vector_stores(id) ON DELETE CASCADE,
    workspace_id                UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    status                      VARCHAR(50) NOT NULL DEFAULT 'in_progress',
    file_counts_in_progress     INTEGER NOT NULL DEFAULT 0,
    file_counts_completed       INTEGER NOT NULL DEFAULT 0,
    file_counts_failed          INTEGER NOT NULL DEFAULT 0,
    file_counts_cancelled       INTEGER NOT NULL DEFAULT 0,
    file_counts_total           INTEGER NOT NULL DEFAULT 0,
    attributes                  JSONB NOT NULL DEFAULT '{}',
    chunking_strategy           JSONB,
    created_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at                TIMESTAMPTZ,
    updated_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for vector_store_file_batches
CREATE INDEX idx_vsfb_vector_store_id ON vector_store_file_batches(vector_store_id);
CREATE INDEX idx_vsfb_workspace_id ON vector_store_file_batches(workspace_id);
CREATE INDEX idx_vsfb_status ON vector_store_file_batches(status);

-- Check constraints for vector_store_file_batches
ALTER TABLE vector_store_file_batches ADD CONSTRAINT chk_vsfb_status
    CHECK (status IN ('in_progress', 'completed', 'cancelled', 'failed'));

ALTER TABLE vector_store_file_batches ADD CONSTRAINT chk_vsfb_file_counts_non_negative
    CHECK (
        file_counts_in_progress >= 0 AND
        file_counts_completed >= 0 AND
        file_counts_failed >= 0 AND
        file_counts_cancelled >= 0 AND
        file_counts_total >= 0
    );

-- Trigger for updated_at
CREATE TRIGGER set_vsfb_updated_at
    BEFORE UPDATE ON vector_store_file_batches
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- =============================================================================
-- Table: vector_store_files
-- =============================================================================
CREATE TABLE vector_store_files (
    id                          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    vector_store_id             UUID NOT NULL REFERENCES vector_stores(id) ON DELETE CASCADE,
    file_id                     UUID NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    workspace_id                UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    batch_id                    UUID REFERENCES vector_store_file_batches(id) ON DELETE SET NULL,
    status                      VARCHAR(50) NOT NULL DEFAULT 'in_progress',
    usage_bytes                 BIGINT NOT NULL DEFAULT 0,
    chunk_count                 INTEGER NOT NULL DEFAULT 0,
    chunking_strategy           JSONB,
    attributes                  JSONB NOT NULL DEFAULT '{}',
    last_error                  JSONB,
    created_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processing_started_at       TIMESTAMPTZ,
    processing_completed_at     TIMESTAMPTZ,
    updated_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Unique constraint: a file can only be in a vector store once
ALTER TABLE vector_store_files ADD CONSTRAINT uq_vsf_vector_store_file
    UNIQUE (vector_store_id, file_id);

-- Indexes for vector_store_files
CREATE INDEX idx_vsf_vector_store_id ON vector_store_files(vector_store_id);
CREATE INDEX idx_vsf_file_id ON vector_store_files(file_id);
CREATE INDEX idx_vsf_workspace_id ON vector_store_files(workspace_id);
CREATE INDEX idx_vsf_batch_id ON vector_store_files(batch_id);
CREATE INDEX idx_vsf_status ON vector_store_files(status);

-- Check constraints for vector_store_files
ALTER TABLE vector_store_files ADD CONSTRAINT chk_vsf_status
    CHECK (status IN ('in_progress', 'completed', 'cancelled', 'failed'));

ALTER TABLE vector_store_files ADD CONSTRAINT chk_vsf_usage_bytes_non_negative
    CHECK (usage_bytes >= 0);

ALTER TABLE vector_store_files ADD CONSTRAINT chk_vsf_chunk_count_non_negative
    CHECK (chunk_count >= 0);

-- Trigger for updated_at
CREATE TRIGGER set_vsf_updated_at
    BEFORE UPDATE ON vector_store_files
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();
