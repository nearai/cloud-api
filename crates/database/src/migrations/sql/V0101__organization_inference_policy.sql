CREATE TABLE organization_inference_policies (
    organization_id UUID PRIMARY KEY REFERENCES organizations(id),
    tee_required BOOLEAN NOT NULL,
    updated_by UUID NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE organization_inference_policy_history (
    id BIGSERIAL PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id),
    tee_required BOOLEAN NOT NULL,
    changed_by UUID NOT NULL,
    reason TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
