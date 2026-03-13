-- Add inference_url column to models table
-- This stores the base URL for the model's inference endpoint (e.g., "http://localhost:8000")
ALTER TABLE models ADD COLUMN inference_url TEXT;

-- Add inference_url column to model_history table for audit tracking
ALTER TABLE model_history ADD COLUMN inference_url TEXT;
