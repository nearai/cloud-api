-- Mark whether an organization's split spend counters include its historical logs.
-- Existing organizations remain NULL until the backfill operator completes them.
-- New balance rows are complete by construction after all usage writers maintain spend counters.

ALTER TABLE organization_balance
    ADD COLUMN spend_counters_ready_at TIMESTAMPTZ;

ALTER TABLE organization_balance
    ALTER COLUMN spend_counters_ready_at SET DEFAULT NOW();

COMMENT ON COLUMN organization_balance.spend_counters_ready_at IS
    'Non-NULL after the spend counter historical reconciliation completed for this organization';
