-- Idempotent usage recording: prevent duplicate charges for the same inference.
--
-- Adds a partial unique index on (organization_id, inference_id) so the same
-- external id within one org can only create a single usage log entry.
-- Scoped per-organization because external IDs are caller-supplied and
-- different orgs may independently use the same id value.
-- NULL inference_ids are excluded (PostgreSQL treats NULLs as distinct in
-- unique indexes, so rows without an inference_id are unaffected).
--
-- Before creating the unique index we must remove pre-existing duplicate
-- rows (from before idempotency was enforced).  The most recent record per
-- (organization_id, inference_id) group is kept (most likely to have
-- complete token counts and stop reason); older duplicates are deleted.
-- organization_balance is NOT reconciled here — that can be done as a
-- separate auditable step after inspecting the impact.

-- Step 1: Remove duplicate (organization_id, inference_id) rows, keeping only
-- the most recent record (largest created_at) per group.
-- Uses DELETE ... USING with a window function instead of NOT IN subquery
-- to avoid materializing a large ID list and reduce lock duration.
DELETE FROM organization_usage_log
USING (
    SELECT id,
           row_number() OVER (PARTITION BY organization_id, inference_id ORDER BY created_at DESC) AS rn
    FROM organization_usage_log
    WHERE inference_id IS NOT NULL
) AS duplicates
WHERE organization_usage_log.id = duplicates.id
  AND duplicates.rn > 1;

-- Step 2: Now that duplicates are gone, create the unique index.
CREATE UNIQUE INDEX IF NOT EXISTS idx_org_usage_org_inference_unique
    ON organization_usage_log(organization_id, inference_id)
    WHERE inference_id IS NOT NULL;
