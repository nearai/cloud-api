-- Refactor model_pricing_history table to model_history
-- Rename table and add all missing columns from models table

-- Step 1: Rename table
ALTER TABLE model_pricing_history RENAME TO model_history;

-- Step 2: Add missing columns from models table
ALTER TABLE model_history ADD COLUMN model_name VARCHAR(500);
ALTER TABLE model_history ADD COLUMN model_icon VARCHAR(500);
ALTER TABLE model_history ADD COLUMN verifiable BOOLEAN;
ALTER TABLE model_history ADD COLUMN is_active BOOLEAN;

-- Step 3: Backfill missing fields from current models table
UPDATE model_history mh
SET
    model_name = m.model_name,
    model_icon = m.model_icon,
    verifiable = m.verifiable,
    is_active = m.is_active
FROM models m
WHERE mh.model_id = m.id;

-- Step 4: Add NOT NULL constraints (after backfill)
ALTER TABLE model_history ALTER COLUMN model_name SET NOT NULL;
ALTER TABLE model_history ALTER COLUMN verifiable SET NOT NULL;
ALTER TABLE model_history ALTER COLUMN is_active SET NOT NULL;
-- model_icon remains NULLABLE

-- Step 5: Rename indexes to match new table name
ALTER INDEX idx_pricing_history_model_id RENAME TO idx_model_history_model_id;
ALTER INDEX idx_pricing_history_effective_from RENAME TO idx_model_history_effective_from;
ALTER INDEX idx_pricing_history_effective_until RENAME TO idx_model_history_effective_until;
ALTER INDEX idx_pricing_history_temporal RENAME TO idx_model_history_temporal;

-- Step 6: Drop the model_pricing_change_trigger and its function
DROP TRIGGER model_pricing_change_trigger ON models;
DROP FUNCTION track_model_pricing_change();

-- Step 7: Drop deprecated changed_by column (replaced by changed_by_user_id and changed_by_user_email)
ALTER TABLE model_history DROP COLUMN changed_by;

-- Step 8: Add user audit tracking columns (for manual history tracking in application code)
ALTER TABLE model_history ADD COLUMN changed_by_user_id UUID REFERENCES users(id);
ALTER TABLE model_history ADD COLUMN changed_by_user_email VARCHAR(255);
-- Note: Existing rows will have NULL for these columns (backward compatible)
