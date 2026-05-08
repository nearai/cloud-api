ALTER TABLE organization_invitations
    ADD COLUMN email_status VARCHAR(50) NOT NULL DEFAULT 'not_attempted'
        CHECK (email_status IN ('not_attempted', 'sent', 'failed', 'skipped')),
    ADD COLUMN email_sent_at TIMESTAMPTZ,
    ADD COLUMN email_last_error TEXT,
    ADD COLUMN email_message_id VARCHAR(255);

CREATE INDEX idx_org_invitations_email_status ON organization_invitations(email_status);

COMMENT ON COLUMN organization_invitations.email_status IS 'Email delivery status: not_attempted, sent, failed, or skipped';
COMMENT ON COLUMN organization_invitations.email_sent_at IS 'Timestamp when invitation email was successfully sent';
COMMENT ON COLUMN organization_invitations.email_last_error IS 'Sanitized last email delivery error, if delivery failed';
COMMENT ON COLUMN organization_invitations.email_message_id IS 'Provider message ID returned after successful delivery';
