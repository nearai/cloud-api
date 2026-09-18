CREATE TABLE database_encryption_jobs (
    id UUID PRIMARY KEY,
    mode TEXT NOT NULL CHECK (mode IN ('dry_run', 'execute', 'verify')),
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'completed', 'failed', 'cancelled')),
    scope JSONB NOT NULL,
    actions JSONB NOT NULL,
    batch_size BIGINT NOT NULL CHECK (batch_size BETWEEN 1 AND 1000),
    max_rows BIGINT,
    cursor JSONB NOT NULL DEFAULT '{}'::jsonb,
    progress JSONB NOT NULL DEFAULT '{}'::jsonb,
    last_error_class TEXT,
    last_error_message TEXT,
    admin_actor UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    cancel_requested_at TIMESTAMPTZ
);
CREATE INDEX idx_database_encryption_jobs_status ON database_encryption_jobs(status, created_at);
CREATE UNIQUE INDEX idx_database_encryption_jobs_active_scope ON database_encryption_jobs ((scope::text)) WHERE status IN ('queued', 'running');

-- Encrypted envelopes are larger than the original catalog VARCHAR limits.
-- Encrypted columns are widened before repository writes or backfill jobs can
-- persist authenticated envelopes.
ALTER TABLE files
    ALTER COLUMN filename TYPE TEXT,
    ALTER COLUMN storage_key TYPE TEXT,
    ALTER COLUMN content_type TYPE TEXT;

ALTER TABLE mcp_connectors
    ALTER COLUMN name TYPE TEXT,
    ALTER COLUMN description TYPE TEXT,
    ALTER COLUMN mcp_server_url TYPE TEXT,
    ALTER COLUMN error_message TYPE TEXT;

-- Keep the structural root marker queryable after response metadata is
-- encrypted. Backfill it before adding the new partial index.
ALTER TABLE responses
    ADD COLUMN is_root_response BOOLEAN NOT NULL DEFAULT FALSE;

-- Keep the legacy metadata-based index until every old binary has drained.
-- Old replicas still use its predicate in ON CONFLICT inference during rolling deploys.
CREATE UNIQUE INDEX idx_responses_is_root_response_unique_per_conversation
    ON responses(conversation_id)
    WHERE is_root_response;
