-- Explicit defaults preserve the security contract independently of which
-- deployments happen to be healthy or available during a request.
CREATE TABLE model_routing_policies (
    model_id UUID PRIMARY KEY REFERENCES models(id),
    default_tee_requirement TEXT NOT NULL CHECK (default_tee_requirement IN ('required','any')),
    mixed_routing_enabled BOOLEAN NOT NULL DEFAULT false,
    max_attempts INTEGER NOT NULL DEFAULT 4 CHECK (max_attempts BETWEEN 1 AND 16),
    deadline_ms BIGINT NOT NULL DEFAULT 120000 CHECK (deadline_ms BETWEEN 100 AND 600000),
    max_attempt_cost_microusd BIGINT CHECK (max_attempt_cost_microusd >= 0),
    max_total_cost_microusd BIGINT CHECK (max_total_cost_microusd >= 0),
    allow_negative_margin BOOLEAN NOT NULL DEFAULT false,
    revision BIGINT NOT NULL DEFAULT 1,
    updated_by UUID,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO model_routing_policies(model_id,default_tee_requirement)
SELECT id, CASE WHEN verifiable OR attestation_supported OR provider_type IN ('chutes','tinfoil')
    THEN 'required' ELSE 'any' END FROM models;

CREATE TABLE model_routing_policy_history (
    id BIGSERIAL PRIMARY KEY,
    model_id UUID NOT NULL REFERENCES models(id),
    changed_by UUID NOT NULL,
    reason TEXT NOT NULL,
    snapshot JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
