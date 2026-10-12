-- Reviewed opening-consumption adjustments, separate from per-request funding.
-- No existing usage or balance is changed by this migration.
CREATE TABLE organization_credit_reconciliations (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    idempotency_key VARCHAR(100) NOT NULL CHECK (length(btrim(idempotency_key)) > 0),
    amounts JSONB NOT NULL CHECK (jsonb_typeof(amounts) = 'object'),
    reason TEXT NOT NULL CHECK (length(btrim(reason)) BETWEEN 1 AND 2000),
    created_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    result JSONB NOT NULL,
    UNIQUE (organization_id, idempotency_key)
);

COMMENT ON TABLE organization_credit_reconciliations IS
    'Append-only audited reclassification of legacy unknown consumption; does not attribute individual usage logs or record invoice settlement. Amounts are nano-USD.';
