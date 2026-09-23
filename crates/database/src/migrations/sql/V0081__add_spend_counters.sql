-- Persist split inference/service spend totals for bounded analytics reads.
-- All values are integer nano-dollars (scale 9, USD).
-- Existing rows are initialized to zero; an out-of-band backfill must run before
-- these columns are used as lifetime totals.
--
-- Every request reads organization_balance, so fail fast instead of queueing
-- behind a long-running read: requests would wait behind this migration's
-- ACCESS EXCLUSIVE lock until then. A timed-out instance retries on restart.
-- refinery runs each migration in its own transaction, so SET LOCAL does not
-- outlive it.
SET LOCAL lock_timeout = '3s';

-- Idempotent, so an image rollback only needs this migration's history row
-- removed; the next deploy re-applies it without touching the existing columns.
-- IF NOT EXISTS only compares names. That is safe here: the columns can only
-- exist from an earlier apply of this migration.
ALTER TABLE organization_balance
    ADD COLUMN IF NOT EXISTS inference_spent BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS service_spent BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS spend_counters_ready_at TIMESTAMPTZ;

-- A re-apply means a pre-counter image served traffic since the last apply. It
-- recorded usage without updating the counters, and the default below stamped
-- any balance row it created as reconciled. Clear readiness so readers use raw
-- history until backfill-spend-counters reconciles each organization again.
-- Matches no rows on the first apply, where the column was just added.
UPDATE organization_balance
SET spend_counters_ready_at = NULL
WHERE spend_counters_ready_at IS NOT NULL;

-- Mark whether an organization's split spend counters include its historical logs.
-- Existing organizations remain NULL until the backfill operator completes them.
-- New balance rows are complete by construction after all usage writers maintain spend counters.
-- Keep this a separate statement: adding the column with this default would stamp
-- every existing organization as reconciled.
ALTER TABLE organization_balance
    ALTER COLUMN spend_counters_ready_at SET DEFAULT NOW();

CREATE TABLE IF NOT EXISTS api_key_spend (
    api_key_id UUID PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
    inference_spent BIGINT NOT NULL DEFAULT 0,
    service_spent BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

COMMENT ON TABLE api_key_spend IS 'Cached per-API-key inference and service spending totals in nano-dollars';
COMMENT ON COLUMN api_key_spend.inference_spent IS 'Cumulative inference spending in nano-dollars (scale 9, USD)';
COMMENT ON COLUMN api_key_spend.service_spent IS 'Cumulative service spending in nano-dollars (scale 9, USD)';
COMMENT ON COLUMN organization_balance.spend_counters_ready_at IS
    'Non-NULL after the spend counter historical reconciliation completed for this organization';
