CREATE TABLE email_notification_deliveries (
    event_type TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    event_key TEXT NOT NULL,
    recipient_email TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (event_type, resource_id, event_key, recipient_email)
);
