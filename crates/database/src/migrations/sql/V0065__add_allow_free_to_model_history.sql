-- Mirror allow_free onto model_history so the audit trail captures whether
-- free-serving was intentionally allowed at each point in time.
-- Defaults to false so existing history rows are consistent with the
-- models.allow_free default added in V0063.
ALTER TABLE model_history ADD COLUMN allow_free BOOLEAN NOT NULL DEFAULT FALSE;
