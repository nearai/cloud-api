-- Platform-level services (e.g. web_search) for forwarding/billing.
-- No seed data; services are created via Admin CRUD.
CREATE TABLE services (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    service_name VARCHAR(100) NOT NULL UNIQUE,
    display_name VARCHAR(255) NOT NULL,
    description TEXT,
    unit VARCHAR(50) NOT NULL CHECK (unit = 'request'),
    cost_per_unit BIGINT NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_services_service_name ON services(service_name);
CREATE INDEX idx_services_is_active ON services(is_active) WHERE is_active = true;

COMMENT ON TABLE services IS 'Platform-level service definitions for forwarding (e.g. web_search). Billing uses cost_per_unit and quantity.';
COMMENT ON COLUMN services.unit IS 'Billing unit; only "request" supported for now.';
COMMENT ON COLUMN services.cost_per_unit IS 'Price per unit in nano-USD (scale 9).';

-- Per-request log for service usage; updates organization_balance.total_spent.
CREATE TABLE organization_service_usage_log (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    api_key_id UUID NOT NULL REFERENCES api_keys(id),
    service_id UUID NOT NULL REFERENCES services(id),
    quantity INTEGER NOT NULL,
    total_cost BIGINT NOT NULL,
    inference_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_org_service_usage_org ON organization_service_usage_log(organization_id);
CREATE INDEX idx_org_service_usage_created ON organization_service_usage_log(organization_id, created_at DESC);
CREATE UNIQUE INDEX idx_org_service_usage_org_inference_unique
    ON organization_service_usage_log(organization_id, inference_id)
    WHERE inference_id IS NOT NULL;

COMMENT ON TABLE organization_service_usage_log IS 'Log of service usage (e.g. web search) per org; total_cost in nano-USD.';
COMMENT ON COLUMN organization_service_usage_log.inference_id IS 'Optional idempotency key; duplicate (org_id, inference_id) skips insert and balance update.';
