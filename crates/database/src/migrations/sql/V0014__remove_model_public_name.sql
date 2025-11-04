-- V14: Remove public_name from models table and migrate to aliases
-- This migration removes the public_name concept and migrates any distinct 
-- public_name values to the model_aliases table

-- Step 1: Migrate public_name values to aliases (only if different from model_name)
-- This creates an alias for any model where public_name != model_name
INSERT INTO model_aliases (alias_name, canonical_model_id, is_active, created_at, updated_at)
SELECT 
    m.public_name,
    m.id,
    m.is_active,
    NOW(),
    NOW()
FROM models m
WHERE m.public_name != m.model_name
ON CONFLICT (alias_name) DO NOTHING; -- Skip if alias already exists

-- Step 2: Drop the unique constraint/index on public_name
DROP INDEX IF EXISTS unique_public_name_active_only;
DROP INDEX IF EXISTS idx_models_public_name;

-- Step 3: Update the trigger to remove public_name from pricing history tracking
DROP TRIGGER IF EXISTS model_pricing_change_trigger ON models;

CREATE TRIGGER model_pricing_change_trigger
AFTER INSERT OR UPDATE OF 
    input_cost_per_token, output_cost_per_token,
    context_length, model_display_name, model_description
ON models
FOR EACH ROW
EXECUTE FUNCTION track_model_pricing_change();

-- Step 4: Drop the public_name column
ALTER TABLE models
DROP COLUMN IF EXISTS public_name;

