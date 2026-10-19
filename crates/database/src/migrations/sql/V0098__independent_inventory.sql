-- Compatibility mapping updates must not revoke independently managed routes.
ALTER TABLE model_provider_routes ADD COLUMN legacy_managed BOOLEAN NOT NULL DEFAULT false;
UPDATE model_provider_routes r SET legacy_managed=true FROM provider_models i
WHERE r.inventory_id=i.id AND r.model_id=i.model_id;
ALTER TABLE provider_models ADD COLUMN manual_provenance TEXT;
