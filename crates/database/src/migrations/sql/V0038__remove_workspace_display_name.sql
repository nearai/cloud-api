-- Remove redundant display_name column from workspaces table
-- The 'name' field will now be the mutable user-facing name
-- The immutable identifier is the 'id' (UUID) column

ALTER TABLE workspaces DROP COLUMN display_name;
