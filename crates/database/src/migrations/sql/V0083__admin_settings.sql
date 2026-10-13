-- Admin-changeable runtime settings: a JSON value per known key. A key with
-- no row, or a field absent from its value, uses the code default.
CREATE TABLE admin_settings (
    key TEXT PRIMARY KEY,
    value JSONB NOT NULL,
    updated_by_user_id UUID NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
