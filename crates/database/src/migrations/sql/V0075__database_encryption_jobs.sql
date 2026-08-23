CREATE TABLE database_encryption_jobs (
    id UUID PRIMARY KEY,
    mode TEXT NOT NULL CHECK (mode IN ('dry_run', 'execute')),
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'completed', 'failed', 'cancelled')),
    scope JSONB NOT NULL,
    actions JSONB NOT NULL,
    batch_size INTEGER NOT NULL,
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
