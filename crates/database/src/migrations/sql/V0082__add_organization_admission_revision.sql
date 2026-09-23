-- Bound the metadata lock wait during a rolling deployment.
SET LOCAL lock_timeout = '3s';

ALTER TABLE organizations
    ADD COLUMN admission_revision BIGINT NOT NULL DEFAULT 0
        CHECK (admission_revision >= 0);

COMMENT ON COLUMN organizations.admission_revision IS
    'Monotonic revision for committed organization and API-key admission state';
