CREATE TABLE email_notification_deliveries (
    event_type TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    event_key TEXT NOT NULL,
    recipient_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    claim_token UUID NOT NULL DEFAULT gen_random_uuid(),
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    dispatch_started_at TIMESTAMPTZ,
    rearm_after_dispatch BOOLEAN NOT NULL DEFAULT FALSE,
    sent_at TIMESTAMPTZ,
    PRIMARY KEY (event_type, resource_id, event_key, recipient_user_id)
);
