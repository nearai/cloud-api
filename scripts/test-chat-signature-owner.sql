-- Test-only bridge for the additive cloud-platform signature-owner migration.
-- The pinned schema image is updated after the cloud-platform change merges.
ALTER TABLE chat_signatures ADD COLUMN IF NOT EXISTS organization_id UUID;
ALTER TABLE chat_signatures ADD COLUMN IF NOT EXISTS workspace_id UUID;
