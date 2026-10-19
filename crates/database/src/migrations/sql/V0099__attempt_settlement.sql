-- Explicit reconciliation evidence is required before an attempt becomes settled.
-- Customer billing records are never modified by procurement reconciliation.
CREATE TABLE inference_attempt_settlements (
    attempt_id UUID PRIMARY KEY REFERENCES inference_attempts(id),
    actual_cost_microusd BIGINT NOT NULL CHECK(actual_cost_microusd >= 0),
    evidence TEXT NOT NULL CHECK(length(evidence) BETWEEN 1 AND 1000),
    changed_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
