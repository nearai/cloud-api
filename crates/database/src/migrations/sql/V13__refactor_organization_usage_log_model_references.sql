-- V13: Refactor organization_usage_log model references
-- 1. Fix model_id to be UUID reference to models table (was VARCHAR with model names)
-- 2. Add denormalized model_name column for query performance and historical accuracy

-- Step 1: Add new columns
ALTER TABLE organization_usage_log
ADD COLUMN model_uuid UUID,
ADD COLUMN model_name VARCHAR(500);

-- Step 2: Populate both new columns by looking up from models table
-- This handles both model_name and public_name matches in the old model_id field
UPDATE organization_usage_log
SET model_uuid = m.id,
    model_name = m.model_name
FROM models m
WHERE organization_usage_log.model_id = m.model_name
   OR organization_usage_log.model_id = m.public_name;

-- Step 3: Verify all records were successfully matched
-- This ensures data integrity before making destructive changes
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM organization_usage_log WHERE model_uuid IS NULL OR model_name IS NULL) THEN
        RAISE EXCEPTION 'Found organization_usage_log records with model_id values that do not match any model. Please fix data before running this migration.';
    END IF;
END $$;

-- Step 4: Make both new columns NOT NULL after verification
ALTER TABLE organization_usage_log
ALTER COLUMN model_uuid SET NOT NULL,
ALTER COLUMN model_name SET NOT NULL;

-- Step 5: Drop the old index on model_id (if exists)
DROP INDEX IF EXISTS idx_org_usage_model;

-- Step 6: Drop the old VARCHAR model_id column
ALTER TABLE organization_usage_log
DROP COLUMN model_id;

-- Step 7: Rename model_uuid to model_id
ALTER TABLE organization_usage_log
RENAME COLUMN model_uuid TO model_id;

-- Step 8: Add foreign key constraint
ALTER TABLE organization_usage_log
ADD CONSTRAINT fk_org_usage_model
FOREIGN KEY (model_id) REFERENCES models(id) ON DELETE RESTRICT;

-- Step 9: Create indexes for both columns
CREATE INDEX idx_org_usage_model ON organization_usage_log(model_id);
CREATE INDEX idx_org_usage_model_name ON organization_usage_log(model_name);

-- Add comments to document the changes
COMMENT ON COLUMN organization_usage_log.model_id IS 'UUID reference to models table (changed from VARCHAR model name in V13)';
COMMENT ON COLUMN organization_usage_log.model_name IS 'Denormalized canonical model name from models table for query performance and historical accuracy';

