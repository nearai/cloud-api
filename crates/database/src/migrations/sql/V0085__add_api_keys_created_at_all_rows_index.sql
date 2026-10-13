-- Support the admin API key listing (GET /v1/admin/api-keys), which includes
-- revoked and inactive keys and orders by (created_at DESC, id DESC).
-- idx_api_keys_active_created_at (V0032) is partial on active, non-deleted
-- keys, so it cannot serve that ordering or created_at range filters over all
-- keys. api_keys is small relative to usage tables, so a plain (non-concurrent)
-- build inside the Refinery transaction is acceptable.
CREATE INDEX IF NOT EXISTS idx_api_keys_created_at_id
    ON api_keys (created_at DESC, id DESC);
