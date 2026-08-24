-- Encrypted envelopes are larger than the original catalog VARCHAR limits.
-- Execute mode is currently gated until repository decrypt-on-read support is
-- available, but widen these columns before any backfill is enabled.
ALTER TABLE database_encryption_jobs
    ALTER COLUMN batch_size TYPE BIGINT;

ALTER TABLE files
    ALTER COLUMN filename TYPE TEXT,
    ALTER COLUMN storage_key TYPE TEXT,
    ALTER COLUMN content_type TYPE TEXT;

ALTER TABLE mcp_connectors
    ALTER COLUMN name TYPE TEXT,
    ALTER COLUMN description TYPE TEXT,
    ALTER COLUMN mcp_server_url TYPE TEXT,
    ALTER COLUMN error_message TYPE TEXT;
