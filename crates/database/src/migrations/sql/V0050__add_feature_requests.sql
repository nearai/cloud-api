CREATE TABLE feature_request_targets (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    kind VARCHAR(50) NOT NULL CHECK (kind IN ('model', 'feature')),
    key VARCHAR(255) NOT NULL,
    title VARCHAR(255) NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'planned', 'in_progress', 'shipped', 'closed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (kind, key)
);

CREATE INDEX idx_feature_request_targets_kind ON feature_request_targets(kind);
CREATE INDEX idx_feature_request_targets_status ON feature_request_targets(status);
CREATE INDEX idx_feature_request_targets_updated ON feature_request_targets(updated_at DESC);

CREATE TABLE feature_request_votes (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    target_id UUID NOT NULL REFERENCES feature_request_targets(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    organization_id UUID REFERENCES organizations(id) ON DELETE SET NULL,
    note TEXT,
    source VARCHAR(100),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (target_id, user_id)
);

CREATE INDEX idx_feature_request_votes_target ON feature_request_votes(target_id);
CREATE INDEX idx_feature_request_votes_user ON feature_request_votes(user_id);
CREATE INDEX idx_feature_request_votes_org ON feature_request_votes(organization_id);
CREATE INDEX idx_feature_request_votes_updated ON feature_request_votes(updated_at DESC);

COMMENT ON TABLE feature_request_targets IS 'Reusable feature-interest targets, including missing model requests and future feature demand signals.';
COMMENT ON TABLE feature_request_votes IS 'Unique user interest signals for feature_request_targets. Repeat submissions update context without inflating demand.';
