-- Add model architecture fields using JSONB arrays for flexibility
-- This follows HuggingFace/OpenRouter conventions for describing model capabilities
--
-- Columns are nullable for now - existing models will have NULL values.
-- A future migration will make these NOT NULL once all models are populated.
-- The API layer omits the architecture field when modalities are NULL.

-- Add input/output modalities to models table (nullable)
ALTER TABLE models ADD COLUMN input_modalities JSONB;
ALTER TABLE models ADD COLUMN output_modalities JSONB;

-- Add to model_history for audit trail
ALTER TABLE model_history ADD COLUMN input_modalities JSONB;
ALTER TABLE model_history ADD COLUMN output_modalities JSONB;

-- GIN indexes for efficient array queries (e.g., find all models that accept images)
-- Using jsonb_path_ops for better performance on containment queries
CREATE INDEX idx_models_input_modalities ON models USING GIN (input_modalities jsonb_path_ops);
CREATE INDEX idx_models_output_modalities ON models USING GIN (output_modalities jsonb_path_ops);
