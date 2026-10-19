-- Provider inventory is separate from the billable model catalog. Discovery
-- never approves, deletes, unarchives, or changes customer prices.
CREATE TABLE inference_providers (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    name TEXT NOT NULL UNIQUE,
    config JSONB NOT NULL CHECK (jsonb_typeof(config) = 'object'),
    credential_env TEXT,
    models_path TEXT,
    archived BOOLEAN NOT NULL DEFAULT false,
    execution_security TEXT NOT NULL DEFAULT 'non_tee' CHECK (execution_security = 'non_tee'),
    revision BIGINT NOT NULL DEFAULT 1,
    last_synced_at TIMESTAMPTZ,
    sync_error TEXT,
    updated_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (NOT (config ? 'api_key') AND NOT (config ? 'model_name'))
);

CREATE TABLE provider_models (
    provider_id UUID NOT NULL REFERENCES inference_providers(id),
    upstream_model TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    available BOOLEAN NOT NULL DEFAULT true,
    model_id UUID REFERENCES models(id),
    approved BOOLEAN NOT NULL DEFAULT false,
    archived BOOLEAN NOT NULL DEFAULT false,
    discovered_reasoning_efforts TEXT[],
    reasoning_efforts_override TEXT[],
    updated_by UUID,
    last_seen_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider_id, upstream_model),
    CHECK (NOT approved OR model_id IS NOT NULL)
);
CREATE INDEX provider_models_catalog_idx ON provider_models(model_id);

CREATE TABLE provider_registry_history (
    id BIGSERIAL PRIMARY KEY,
    provider_id UUID NOT NULL REFERENCES inference_providers(id),
    upstream_model TEXT,
    changed_by UUID NOT NULL,
    snapshot JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Legacy per-model configuration remains readable until an admin explicitly
-- maps that model into the registry. No credentials are copied by this migration.

ALTER TABLE models ADD COLUMN provider_registry_managed BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE models ADD COLUMN archived_at TIMESTAMPTZ;
ALTER TABLE models ADD COLUMN archive_was_active BOOLEAN;
CREATE FUNCTION preserve_model_archive() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.archived_at IS NOT NULL THEN
        NEW.is_active := false;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER preserve_model_archive BEFORE INSERT OR UPDATE ON models
    FOR EACH ROW EXECUTE FUNCTION preserve_model_archive();
ALTER TABLE model_history ADD COLUMN archived_at TIMESTAMPTZ;
ALTER TABLE provider_models ADD COLUMN provenance TEXT NOT NULL DEFAULT 'manual'
    CHECK (provenance IN ('manual', 'discovered'));
ALTER TABLE inference_providers ADD COLUMN next_sync_at TIMESTAMPTZ NOT NULL DEFAULT now();
