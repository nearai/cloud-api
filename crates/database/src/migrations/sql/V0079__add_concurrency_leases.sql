-- One row per in-flight request, so replicas count against a shared limit
-- rather than one each. Rows outlive a dead replica and are swept on expiry.
CREATE TABLE concurrency_leases (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    model_id UUID NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    instance_id TEXT NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX idx_concurrency_leases_org_model
    ON concurrency_leases (organization_id, model_id, expires_at);

CREATE INDEX idx_concurrency_leases_expires_at
    ON concurrency_leases (expires_at);
