-- Audit table for planned model deprecation notification emails.
-- One row is recorded per affected user/org membership; the service sends at
-- most one email per distinct recipient email for a given deprecation.
CREATE TABLE model_deprecation_email_deliveries (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    model_id UUID NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    model_name VARCHAR(500) NOT NULL,
    model_display_name VARCHAR(255) NOT NULL,
    successor_model_name VARCHAR(500) NOT NULL,
    deprecation_date TIMESTAMPTZ NOT NULL,
    recipient_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    recipient_email VARCHAR(255) NOT NULL,
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    organization_name VARCHAR(255) NOT NULL,
    status VARCHAR(20) NOT NULL CHECK (status IN ('sent', 'failed', 'skipped')),
    email_sent_at TIMESTAMPTZ,
    email_message_id TEXT,
    email_last_error TEXT,
    initiated_by_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    initiated_by_user_email VARCHAR(255),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (
        model_id,
        successor_model_name,
        deprecation_date,
        recipient_user_id,
        organization_id
    )
);

CREATE INDEX idx_model_deprecation_email_model
    ON model_deprecation_email_deliveries(model_id, deprecation_date DESC);

CREATE INDEX idx_model_deprecation_email_recipient
    ON model_deprecation_email_deliveries(recipient_email);

CREATE INDEX idx_model_deprecation_email_status
    ON model_deprecation_email_deliveries(status);
