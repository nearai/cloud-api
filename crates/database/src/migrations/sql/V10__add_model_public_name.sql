-- V10: Add public_name to models table
-- This migration adds a public-facing model name field to the models table
-- The public_name is what API consumers see, while model_name remains the internal/canonical name

-- Add public_name column to models table
ALTER TABLE models
ADD COLUMN public_name VARCHAR(500);

-- Populate public_name with model_name as default (temporary)
UPDATE models
SET public_name = model_name;

-- Make public_name NOT NULL and UNIQUE after populating
ALTER TABLE models
ALTER COLUMN public_name SET NOT NULL;

ALTER TABLE models
ADD CONSTRAINT unique_public_name UNIQUE (public_name);

-- Add index for efficient lookups by public_name
CREATE INDEX idx_models_public_name ON models(public_name);

-- Update the trigger to include public_name in pricing history tracking
DROP TRIGGER IF EXISTS model_pricing_change_trigger ON models;

CREATE TRIGGER model_pricing_change_trigger
AFTER INSERT OR UPDATE OF 
    input_cost_per_token, output_cost_per_token,
    context_length, model_display_name, model_description, public_name
ON models
FOR EACH ROW
EXECUTE FUNCTION track_model_pricing_change();

