CREATE TABLE email_notification_deliveries (
    event_type TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    event_key TEXT NOT NULL,
    recipient_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    sent_at TIMESTAMPTZ,
    PRIMARY KEY (event_type, resource_id, event_key, recipient_user_id)
);
