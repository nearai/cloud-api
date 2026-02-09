-- Idempotent usage recording: prevent duplicate charges for the same inference.
--
-- Adds a partial unique index on (organization_id, inference_id) so the same
-- external id within one org can only create a single usage log entry.
-- Scoped per-organization because external IDs are caller-supplied and
-- different orgs may independently use the same id value.
-- NULL inference_ids are excluded (PostgreSQL treats NULLs as distinct in
-- unique indexes, so rows without an inference_id are unaffected).

CREATE UNIQUE INDEX IF NOT EXISTS idx_org_usage_org_inference_unique
    ON organization_usage_log(organization_id, inference_id)
    WHERE inference_id IS NOT NULL;
