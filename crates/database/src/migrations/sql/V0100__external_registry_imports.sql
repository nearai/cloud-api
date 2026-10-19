-- A staged import keeps sanitized rollback configuration and stable identities.
-- Secrets are referenced, never copied into the import/audit snapshot.
CREATE TABLE external_registry_imports (
    model_id UUID PRIMARY KEY REFERENCES models(id),
    deployment_id UUID NOT NULL REFERENCES inference_providers(id),
    legacy_config JSONB NOT NULL CHECK (NOT (legacy_config ? 'api_key')),
    state TEXT NOT NULL CHECK(state IN ('shadow','active','rollback')),
    updated_by UUID NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
