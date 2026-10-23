ALTER TABLE email_notification_deliveries
    ADD COLUMN rearm_after_dispatch BOOLEAN NOT NULL DEFAULT FALSE;
