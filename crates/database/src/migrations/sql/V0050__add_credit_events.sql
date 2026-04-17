CREATE TABLE credit_events (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    name VARCHAR(255) NOT NULL,
    description TEXT,
    credit_amount BIGINT NOT NULL,
    currency VARCHAR(10) NOT NULL DEFAULT 'USD',
    max_claims INTEGER,
    claim_count INTEGER NOT NULL DEFAULT 0,
    starts_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    claim_deadline TIMESTAMPTZ,
    credit_expires_at TIMESTAMPTZ NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_by_user_id UUID REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_credit_events_active ON credit_events(is_active) WHERE is_active = true;
CREATE INDEX idx_credit_events_created_by ON credit_events(created_by_user_id);

CREATE TABLE credit_event_codes (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    credit_event_id UUID NOT NULL REFERENCES credit_events(id) ON DELETE CASCADE,
    code VARCHAR(64) NOT NULL,
    is_claimed BOOLEAN NOT NULL DEFAULT false,
    claimed_by_user_id UUID REFERENCES users(id),
    claimed_by_near_account_id VARCHAR(255),
    claimed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(code)
);

CREATE INDEX idx_credit_event_codes_event ON credit_event_codes(credit_event_id);
CREATE INDEX idx_credit_event_codes_code ON credit_event_codes(code) WHERE is_claimed = false;

CREATE TABLE credit_claims (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    credit_event_id UUID NOT NULL REFERENCES credit_events(id) ON DELETE CASCADE,
    code_id UUID NOT NULL REFERENCES credit_event_codes(id),
    near_account_id VARCHAR(255) NOT NULL,
    user_id UUID NOT NULL REFERENCES users(id),
    organization_id UUID NOT NULL REFERENCES organizations(id),
    organization_limit_id UUID REFERENCES organization_limits_history(id),
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_credit_claims_event ON credit_claims(credit_event_id);
CREATE INDEX idx_credit_claims_near_account ON credit_claims(near_account_id);
CREATE INDEX idx_credit_claims_user ON credit_claims(user_id);
CREATE INDEX idx_credit_claims_org ON credit_claims(organization_id);