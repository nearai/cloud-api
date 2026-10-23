-- Recommended replacement for a model with a planned deprecation.
--
-- Until now the successor only lived in `model_deprecation_email_deliveries`
-- and in the free-text `change_reason`, so nothing could tell API users which
-- model to move to. It is sent in the `x-model-successor` response header,
-- next to `x-model-deprecation-date`, and exposed on `GET /v1/models`.
--
-- Canonical model name. Cleared together with `deprecation_date`. NULL = none.
ALTER TABLE models ADD COLUMN successor_model_name VARCHAR(500);

-- Mirror onto model_history so admin edits are audited (V0053 precedent).
ALTER TABLE model_history ADD COLUMN successor_model_name VARCHAR(500);
