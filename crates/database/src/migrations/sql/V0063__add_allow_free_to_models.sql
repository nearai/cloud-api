-- Add allow_free flag to the models table.
-- When true, a model with zero pricing may be activated without triggering
-- the activation pricing gate. Defaults to false so existing rows are
-- governed by the gate until an operator explicitly opts out.
ALTER TABLE models ADD COLUMN allow_free BOOLEAN NOT NULL DEFAULT FALSE;

-- Mirror allow_free onto model_history so the audit trail captures whether
-- free-serving was intentionally allowed at each point in time.
ALTER TABLE model_history ADD COLUMN allow_free BOOLEAN NOT NULL DEFAULT FALSE;
