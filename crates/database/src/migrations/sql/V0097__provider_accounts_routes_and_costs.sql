-- Extend the registry introduced in V0095 rather than adding a second loader.
-- inference_providers are deployment endpoints; provider_accounts are the
-- vendor/account identity that may own several independently secured endpoints.
CREATE TABLE provider_accounts (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    name TEXT NOT NULL UNIQUE,
    vendor TEXT NOT NULL,
    account_reference TEXT NOT NULL,
    archived BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO provider_accounts(id,name,vendor,account_reference)
SELECT id,name,'unspecified',name FROM inference_providers;
ALTER TABLE inference_providers ADD COLUMN account_id UUID REFERENCES provider_accounts(id);
UPDATE inference_providers SET account_id=id;
ALTER TABLE inference_providers ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE inference_providers ADD COLUMN quota_group TEXT;
ALTER TABLE inference_providers ADD COLUMN enabled BOOLEAN NOT NULL DEFAULT true;

ALTER TABLE provider_models ADD COLUMN id UUID NOT NULL DEFAULT uuid_generate_v4();
ALTER TABLE provider_models ADD CONSTRAINT provider_models_stable_id UNIQUE(id);
ALTER TABLE provider_models ADD COLUMN first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE TABLE model_provider_routes (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    model_id UUID NOT NULL REFERENCES models(id),
    inventory_id UUID NOT NULL REFERENCES provider_models(id),
    approved BOOLEAN NOT NULL DEFAULT false,
    enabled BOOLEAN NOT NULL DEFAULT true,
    archived BOOLEAN NOT NULL DEFAULT false,
    reasoning_efforts_override TEXT[],
    context_limit INTEGER CHECK (context_limit > 0),
    output_limit INTEGER CHECK (output_limit > 0),
    role TEXT NOT NULL DEFAULT 'primary' CHECK (role IN ('primary','fallback')),
    revision BIGINT NOT NULL DEFAULT 1,
    updated_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(model_id,inventory_id)
);
INSERT INTO model_provider_routes(model_id,inventory_id,approved,archived,reasoning_efforts_override,updated_by)
SELECT model_id,id,approved,archived,reasoning_efforts_override,updated_by
FROM provider_models WHERE model_id IS NOT NULL;
CREATE INDEX model_provider_routes_inventory_idx ON model_provider_routes(inventory_id);

CREATE TABLE route_cost_profiles (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    route_id UUID NOT NULL REFERENCES model_provider_routes(id),
    version BIGINT NOT NULL CHECK (version > 0),
    currency TEXT NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    billing_unit TEXT NOT NULL CHECK (billing_unit='million_tokens'),
    input_microunits BIGINT NOT NULL CHECK (input_microunits >= 0),
    output_microunits BIGINT NOT NULL CHECK (output_microunits >= 0),
    cache_read_microunits BIGINT CHECK (cache_read_microunits >= 0),
    cache_write_microunits BIGINT CHECK (cache_write_microunits >= 0),
    effective_from TIMESTAMPTZ NOT NULL DEFAULT now(),
    effective_until TIMESTAMPTZ,
    provenance TEXT NOT NULL,
    CHECK (effective_until IS NULL OR effective_until > effective_from),
    UNIQUE(route_id,version)
);
CREATE UNIQUE INDEX route_cost_profiles_current ON route_cost_profiles(route_id) WHERE effective_until IS NULL;
CREATE INDEX route_cost_profiles_effective ON route_cost_profiles(route_id,effective_from);

CREATE TABLE registry_entity_history (
    id BIGSERIAL PRIMARY KEY,
    entity_kind TEXT NOT NULL CHECK (entity_kind IN ('account','route','cost_profile','import')),
    entity_id UUID NOT NULL,
    changed_by UUID NOT NULL,
    reason TEXT NOT NULL,
    snapshot JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX registry_entity_history_entity ON registry_entity_history(entity_kind,entity_id,created_at);

CREATE TABLE inference_attempts (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    request_id UUID NOT NULL,
    accepted_request_id UUID,
    usage_cost_microusd BIGINT CHECK (usage_cost_microusd >= 0),
    attempt_index INTEGER NOT NULL CHECK (attempt_index > 0),
    route_id UUID REFERENCES model_provider_routes(id),
    deployment_id UUID REFERENCES inference_providers(id),
    route_revision BIGINT,
    cost_profile_id UUID REFERENCES route_cost_profiles(id),
    effective_tee TEXT NOT NULL CHECK (effective_tee IN ('required','any')),
    reasoning_effort TEXT,
    outcome TEXT NOT NULL,
    fallback_reason TEXT,
    estimated_cost_microusd BIGINT CHECK (estimated_cost_microusd >= 0),
    actual_cost_microusd BIGINT CHECK (actual_cost_microusd >= 0),
    cost_status TEXT NOT NULL CHECK (cost_status IN ('unknown','estimated','settled')),
    input_tokens BIGINT,
    output_tokens BIGINT,
    latency_ms BIGINT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    UNIQUE(request_id,attempt_index)
);
CREATE INDEX inference_attempts_deployment_time ON inference_attempts(deployment_id,started_at);

CREATE INDEX inference_attempts_accepted_request ON inference_attempts(accepted_request_id);

-- Ephemeral shared circuit leases; no health writes touch catalog entities.
CREATE TABLE provider_quota_health (
    quota_group TEXT PRIMARY KEY,
    failures BIGINT NOT NULL DEFAULT 1,
    last_failed_at TIMESTAMPTZ NOT NULL,
    cooldown_until TIMESTAMPTZ NOT NULL,
    probe_until TIMESTAMPTZ
);
