-- V12: Fix public_name unique constraint to only apply to active models
-- This allows reusing public_name after a model has been soft-deleted (is_active = false)

-- Drop the existing unique constraint
ALTER TABLE models
DROP CONSTRAINT IF EXISTS unique_public_name;

-- Create a partial unique index that only applies to active models
-- This allows multiple inactive models to have the same public_name,
-- but ensures active models have unique public_name values
CREATE UNIQUE INDEX unique_public_name_active_only
ON models(public_name)
WHERE is_active = true;

-- The existing index on public_name can remain for general lookups
-- (it's not unique, so it won't conflict with our partial unique index)
