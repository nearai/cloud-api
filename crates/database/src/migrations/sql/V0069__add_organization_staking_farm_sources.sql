CREATE TABLE organization_staking_farm_sources (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id),
    near_account_id TEXT NOT NULL,
    network_id TEXT NOT NULL,
    contract_id TEXT NOT NULL,
    farm_product_id TEXT NOT NULL,
    farm_price_id TEXT,
    credit_nano_usd_per_reward_unit BIGINT NOT NULL,
    status TEXT NOT NULL,
    sync_status TEXT NOT NULL DEFAULT 'never_synced',
    last_sync_error TEXT,
    created_by_user_id UUID REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_synced_at TIMESTAMPTZ,
    last_synced_accumulated_reward_units_24 NUMERIC(39,0),
    last_synced_pending_reward_units_24 NUMERIC(39,0),
    last_synced_reward_units_24 NUMERIC(39,0),
    last_synced_credit_nano_usd BIGINT,
    active_positions JSONB NOT NULL DEFAULT '[]'::jsonb,
    CONSTRAINT organization_staking_farm_sources_status_check
        CHECK (status IN ('active', 'disconnected')),
    CONSTRAINT organization_staking_farm_sources_sync_status_check
        CHECK (sync_status IN ('never_synced', 'synced', 'stale', 'failed')),
    CONSTRAINT organization_staking_farm_sources_reward_units_nonnegative_check
        CHECK (
            (last_synced_accumulated_reward_units_24 IS NULL OR last_synced_accumulated_reward_units_24 >= 0)
            AND (last_synced_pending_reward_units_24 IS NULL OR last_synced_pending_reward_units_24 >= 0)
            AND (last_synced_reward_units_24 IS NULL OR last_synced_reward_units_24 >= 0)
        ),
    CONSTRAINT organization_staking_farm_sources_credit_nonnegative_check
        CHECK (last_synced_credit_nano_usd IS NULL OR last_synced_credit_nano_usd >= 0),
    CONSTRAINT organization_staking_farm_sources_conversion_nonnegative_check
        CHECK (credit_nano_usd_per_reward_unit >= 0),
    UNIQUE (near_account_id, network_id, contract_id),
    UNIQUE (organization_id, network_id, contract_id, farm_product_id)
);

CREATE INDEX idx_organization_staking_farm_sources_active_sync
    ON organization_staking_farm_sources (status, sync_status, last_synced_at)
    WHERE status = 'active';

CREATE INDEX idx_organization_limits_history_active_credit_type
    ON organization_limits_history (organization_id, credit_type)
    WHERE effective_until IS NULL;

CREATE UNIQUE INDEX idx_organization_limits_history_active_staking_farm_unique
    ON organization_limits_history (organization_id, credit_type)
    WHERE effective_until IS NULL AND credit_type = 'staking_farm';
