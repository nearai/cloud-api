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
-- A grant is a relationship between two rows, written by admins, so it gets
-- its own table. No existing column can carry it: `organizations.settings` is
-- customer-editable and must never hold access state.
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

-- The audit trail of the table above. A revocation deletes its grant row, so
-- the table alone cannot say who had access when, or who took it away.
--
-- Append-only: one row per grant and one per revocation, written in the same
-- statement as the change, and only when the change happened (granting a
-- grant that exists, or revoking one that does not, writes nothing).
--
-- The foreign keys carry no ON DELETE action, like the role audit log
-- (V0077): an audit row keeps the rows it names from being hard deleted. User
-- erasure rewrites the `users` row in place and keeps its id, and this table
-- holds ids only, so there is nothing for it to scrub here.
CREATE TABLE organization_model_access_log (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id),
    model_id UUID NOT NULL REFERENCES models(id),
    action VARCHAR(10) NOT NULL CHECK (action IN ('grant', 'revoke')),
    -- The admin who made the change. NULL for a revocation made by the
    -- platform: deleting an organization removes its grants.
    acting_user_id UUID REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_organization_model_access_log_model_created_at
    ON organization_model_access_log (model_id, created_at DESC);

CREATE INDEX idx_organization_model_access_log_org_created_at
    ON organization_model_access_log (organization_id, created_at DESC);
