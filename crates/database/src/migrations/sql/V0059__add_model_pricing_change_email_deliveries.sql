-- Audit table for scheduled pricing change notification emails.
-- One row is recorded per affected user/org membership; the service sends at
-- most one consolidated email per distinct recipient email for a given batch,
-- listing every model in the batch that the recipient's org(s) used.
CREATE TABLE model_pricing_change_email_deliveries (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    batch_id UUID NOT NULL,
    recipient_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    recipient_email VARCHAR(255) NOT NULL,
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    organization_name VARCHAR(255) NOT NULL,
    -- Canonical model names included in the email this recipient received.
    model_names TEXT[] NOT NULL,
    status VARCHAR(20) NOT NULL CHECK (status IN ('sent', 'failed', 'skipped')),
    email_sent_at TIMESTAMPTZ,
    email_message_id TEXT,
    email_last_error TEXT,
    initiated_by_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    initiated_by_user_email VARCHAR(255),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (batch_id, recipient_user_id, organization_id)
);

CREATE INDEX idx_pricing_change_email_recipient
    ON model_pricing_change_email_deliveries(recipient_email);

CREATE INDEX idx_pricing_change_email_status
    ON model_pricing_change_email_deliveries(status);
