-- V75: Clean up workspaces and API keys orphaned by an organization soft-delete.
--
-- Deleting an organization only flipped `organizations.is_active = false`. Its
-- workspaces stayed active, so `/v1/users/me` kept returning them while the
-- organization itself disappeared from the org list — clients that derive the
-- selected organization from that workspace list got pinned to a dead org and
-- every subsequent call 404'd ("Organization not found" / "Workspace not
-- found"). The workspaces were already unusable: every workspace lookup joins
-- `organizations` with `is_active = true`.
--
-- API keys under those workspaces were already rejected at auth time (the
-- middleware resolves the workspace through the same join), but their rows
-- still read as active; deactivate them so `is_active` is truthful.

UPDATE api_keys
SET is_active = false
WHERE is_active = true
  AND workspace_id IN (
      SELECT w.id
      FROM workspaces w
      JOIN organizations o ON w.organization_id = o.id
      WHERE o.is_active = false
  );

UPDATE workspaces w
SET is_active = false,
    updated_at = NOW()
FROM organizations o
WHERE w.organization_id = o.id
  AND o.is_active = false
  AND w.is_active = true;
