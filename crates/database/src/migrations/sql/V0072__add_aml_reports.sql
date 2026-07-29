CREATE TABLE aml_reports (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    flow TEXT NOT NULL,
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    address_type TEXT NOT NULL,
    risk_level TEXT NOT NULL CHECK (risk_level IN ('LOW', 'MEDIUM', 'HIGH', 'UNKNOWN')),
    score INTEGER,
    report_id TEXT,
    reason TEXT,
    provider_report_time TIMESTAMPTZ,
    result_json JSONB NOT NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_aml_reports_account_active
    ON aml_reports (provider, address_type, account_id, active, created_at DESC);

CREATE TABLE aml_allowlisted_accounts (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    account_id TEXT NOT NULL,
    address_type TEXT NOT NULL,
    reason TEXT,
    created_by_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (account_id, address_type)
);
