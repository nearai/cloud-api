-- Immutable posting-time attribution for inference and platform-service usage.
-- Existing rows remain NULL/unknown; the migration deliberately does not
-- invent a historical funding split from aggregate spend.

ALTER TABLE organization_usage_log
    ADD COLUMN funded_amount BIGINT,
    ADD COLUMN unfunded_amount BIGINT,
    ADD COLUMN allocation_policy_version VARCHAR(50),
    ADD CONSTRAINT organization_usage_funding_nonnegative
        CHECK ((funded_amount IS NULL AND unfunded_amount IS NULL)
            OR (funded_amount IS NOT NULL AND unfunded_amount IS NOT NULL
                AND funded_amount >= 0 AND unfunded_amount >= 0)) NOT VALID,
    ADD CONSTRAINT organization_usage_funding_reconciles
        CHECK ((funded_amount IS NULL AND unfunded_amount IS NULL)
            OR (funded_amount IS NOT NULL AND unfunded_amount IS NOT NULL
                AND funded_amount + unfunded_amount = total_cost)) NOT VALID;

ALTER TABLE organization_service_usage_log
    ADD COLUMN funded_amount BIGINT,
    ADD COLUMN unfunded_amount BIGINT,
    ADD COLUMN allocation_policy_version VARCHAR(50),
    ADD CONSTRAINT organization_service_usage_funding_nonnegative
        CHECK ((funded_amount IS NULL AND unfunded_amount IS NULL)
            OR (funded_amount IS NOT NULL AND unfunded_amount IS NOT NULL
                AND funded_amount >= 0 AND unfunded_amount >= 0)) NOT VALID,
    ADD CONSTRAINT organization_service_usage_funding_reconciles
        CHECK ((funded_amount IS NULL AND unfunded_amount IS NULL)
            OR (funded_amount IS NOT NULL AND unfunded_amount IS NOT NULL
                AND funded_amount + unfunded_amount = total_cost)) NOT VALID;

-- Snapshot the rollout-era unattributed spend once. Recomputing it from the
-- lifetime usage tables on every charge would make the accounting lock slower
-- as an organization's history grows. New organizations keep the zero default.
ALTER TABLE organization_balance
    ADD COLUMN legacy_unattributed_amount BIGINT NOT NULL DEFAULT 0
        CHECK (legacy_unattributed_amount >= 0),
    ADD COLUMN unresolved_unfunded_amount BIGINT NOT NULL DEFAULT 0
        CHECK (unresolved_unfunded_amount >= 0);

UPDATE organization_balance
SET legacy_unattributed_amount = GREATEST(total_spent, 0);

CREATE TABLE usage_credit_allocations (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    inference_usage_id UUID REFERENCES organization_usage_log(id) ON DELETE CASCADE,
    service_usage_id UUID REFERENCES organization_service_usage_log(id) ON DELETE CASCADE,
    credit_type VARCHAR(50) NOT NULL
        CHECK (credit_type IN ('grant', 'postpay', 'staking_farm', 'payment')),
    amount BIGINT NOT NULL CHECK (amount > 0),
    organization_limit_id UUID NOT NULL REFERENCES organization_limits_history(id),
    source VARCHAR(100),
    policy_version VARCHAR(50) NOT NULL,
    priority_position SMALLINT NOT NULL CHECK (priority_position >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT usage_credit_allocations_one_parent
        CHECK ((inference_usage_id IS NOT NULL)::integer + (service_usage_id IS NOT NULL)::integer = 1)
);

-- Bounded accounting counters used by usage posting and admission checks.
-- The immutable allocation ledger remains the source for history/reporting.
CREATE TABLE organization_credit_consumption (
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    credit_type VARCHAR(50) NOT NULL
        CHECK (credit_type IN ('grant', 'postpay', 'staking_farm', 'payment')),
    amount BIGINT NOT NULL DEFAULT 0 CHECK (amount >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (organization_id, credit_type)
);

CREATE UNIQUE INDEX usage_credit_allocations_inference_type_unique
    ON usage_credit_allocations(inference_usage_id, credit_type)
    WHERE inference_usage_id IS NOT NULL;
CREATE UNIQUE INDEX usage_credit_allocations_service_type_unique
    ON usage_credit_allocations(service_usage_id, credit_type)
    WHERE service_usage_id IS NOT NULL;
CREATE INDEX usage_credit_allocations_org_type
    ON usage_credit_allocations(organization_id, credit_type);
CREATE INDEX usage_credit_allocations_inference
    ON usage_credit_allocations(inference_usage_id)
    WHERE inference_usage_id IS NOT NULL;
CREATE INDEX usage_credit_allocations_service
    ON usage_credit_allocations(service_usage_id)
    WHERE service_usage_id IS NOT NULL;

-- Corrections and write-offs are additive audit records. They never update a
-- usage row or its original posting-time allocations.
CREATE TABLE usage_credit_adjustments (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    inference_usage_id UUID REFERENCES organization_usage_log(id) ON DELETE CASCADE,
    service_usage_id UUID REFERENCES organization_service_usage_log(id) ON DELETE CASCADE,
    adjustment_type VARCHAR(20) NOT NULL
        CHECK (adjustment_type IN ('correction', 'writeoff')),
    amount BIGINT NOT NULL CHECK (amount > 0),
    unfunded_amount_reversed BIGINT NOT NULL
        CHECK (unfunded_amount_reversed >= 0 AND unfunded_amount_reversed <= amount),
    reason TEXT NOT NULL CHECK (LENGTH(BTRIM(reason)) > 0),
    idempotency_key VARCHAR(100) NOT NULL CHECK (LENGTH(BTRIM(idempotency_key)) > 0),
    changed_by_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    changed_by_user_email VARCHAR(255),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT usage_credit_adjustments_one_parent
        CHECK ((inference_usage_id IS NOT NULL)::integer + (service_usage_id IS NOT NULL)::integer = 1),
    CONSTRAINT usage_credit_adjustments_idempotency_unique
        UNIQUE (organization_id, idempotency_key)
);

CREATE TABLE usage_credit_allocation_reversals (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    adjustment_id UUID NOT NULL REFERENCES usage_credit_adjustments(id) ON DELETE CASCADE,
    allocation_id UUID NOT NULL REFERENCES usage_credit_allocations(id) ON DELETE CASCADE,
    amount BIGINT NOT NULL CHECK (amount > 0),
    CONSTRAINT usage_credit_allocation_reversal_unique
        UNIQUE (adjustment_id, allocation_id)
);

CREATE INDEX usage_credit_adjustments_inference
    ON usage_credit_adjustments(inference_usage_id)
    WHERE inference_usage_id IS NOT NULL;
CREATE INDEX usage_credit_adjustments_service
    ON usage_credit_adjustments(service_usage_id)
    WHERE service_usage_id IS NOT NULL;
CREATE INDEX usage_credit_adjustments_org
    ON usage_credit_adjustments(organization_id);
CREATE INDEX usage_credit_allocation_reversals_allocation
    ON usage_credit_allocation_reversals(allocation_id);

COMMENT ON TABLE usage_credit_allocations IS
    'Immutable posting-time funding split for inference and service usage; amounts are nano-USD.';
COMMENT ON TABLE usage_credit_adjustments IS
    'Audited corrections and unfunded write-offs linked to immutable usage rows.';
COMMENT ON COLUMN organization_usage_log.funded_amount IS
    'Attributed nano-USD; NULL means legacy usage with unknown funding.';
COMMENT ON COLUMN organization_usage_log.unfunded_amount IS
    'Nano-USD overage recorded after execution; NULL means legacy/unknown funding.';
