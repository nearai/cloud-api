-- V75: Clean up children orphaned by an organization soft-delete.
--
-- Deleting an organization only flipped `organizations.is_active = false`. Its
-- workspaces stayed active, so `/v1/users/me` kept returning them while the
-- organization itself disappeared from the org list — clients that derive the
-- selected organization from that workspace list got pinned to a dead org and
-- every subsequent call 404'd ("Organization not found" / "Workspace not
-- found"). The workspaces were already unusable: every workspace lookup joins
-- `organizations` with `is_active = true`.
--
-- The credentials underneath them did not fail closed when this migration was
-- introduced:
--
--   * API keys still authenticated on the routes that validate the key alone
--     without resolving workspace/organization context (files, conversations,
--     attestation report/signature).
--   * Reporting tokens were validated on hash, revocation and expiry only —
--     nothing joined the organization — so they kept reading usage data.
--
-- Revoke both, matching the conventions of their normal revoke paths
-- (`deleted_at` for API keys, `revoked_at` for reporting tokens).

UPDATE api_keys
SET is_active = false,
    deleted_at = COALESCE(deleted_at, NOW())
WHERE (is_active = true OR deleted_at IS NULL)
  AND workspace_id IN (
      SELECT w.id
      FROM workspaces w
      JOIN organizations o ON w.organization_id = o.id
      WHERE o.is_active = false
  );

UPDATE organization_reporting_tokens t
SET revoked_at = NOW()
FROM organizations o
WHERE t.organization_id = o.id
  AND o.is_active = false
  AND t.revoked_at IS NULL;

UPDATE workspaces w
SET is_active = false,
    updated_at = NOW()
FROM organizations o
WHERE w.organization_id = o.id
  AND o.is_active = false
  AND w.is_active = true;
