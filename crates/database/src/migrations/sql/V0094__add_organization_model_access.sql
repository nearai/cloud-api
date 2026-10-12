-- Per-organization access to unlisted models.
--
-- V0091 added `models.is_listed`. It only kept a model out of the public
-- catalogs: anyone who knew the name of an unlisted model could still call it.
-- From this migration on an unlisted model is a restricted model. An
-- organization may use it through this API only while it holds a row here (a
-- grant). Everyone else gets the answer they would get for a model that does
-- not exist.
--
-- Rules, enforced where a model is resolved for a caller:
-- - A listed model is open to everyone. Grants on it have no effect, so access
--   can be prepared before the model is unlisted.
-- - A grant counts only while both the organization and the model are active.
--
-- A grant is a relationship between two rows, written by admins and audited,
-- so it gets its own table. No existing column can carry it:
-- `organizations.settings` is customer-editable and must never hold access
-- state.
CREATE TABLE organization_model_access (
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    model_id UUID NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- The admin who granted access. Organizations, models and users are soft
    -- deleted, so the cascades above and this SET NULL are for a manual hard
    -- delete only (V0058, V0086 precedent).
    created_by_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    PRIMARY KEY (organization_id, model_id)
);

-- The primary key serves "does this organization hold a grant for this
-- model" and "which models may this organization use". This index serves the
-- other direction: "which organizations may use this model".
CREATE INDEX idx_organization_model_access_model_id
    ON organization_model_access (model_id);
