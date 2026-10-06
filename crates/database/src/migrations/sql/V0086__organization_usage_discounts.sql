-- Organization inference discounts. Existing prices and funding entries remain auditable.
CREATE TABLE organization_usage_discounts (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL UNIQUE REFERENCES organizations(id) ON DELETE CASCADE,
    discount_basis_points INTEGER NOT NULL CHECK (discount_basis_points BETWEEN 1 AND 10000),
    apply_since TIMESTAMPTZ,
    saved_at TIMESTAMPTZ NOT NULL,
    created_by UUID NOT NULL REFERENCES users(id),
    status TEXT NOT NULL CHECK (status IN ('applying', 'active')),
    processed_count BIGINT NOT NULL DEFAULT 0 CHECK (processed_count >= 0),
    cursor_created_at TIMESTAMPTZ,
    cursor_usage_id UUID,
    CHECK (apply_since IS NULL OR apply_since <= saved_at)
);
CREATE INDEX organization_usage_discounts_pending ON organization_usage_discounts(saved_at)
    WHERE status = 'applying';

CREATE TABLE usage_discount_adjustments (
    rule_id UUID NOT NULL REFERENCES organization_usage_discounts(id) ON DELETE CASCADE,
    usage_id UUID NOT NULL REFERENCES organization_usage_log(id) ON DELETE CASCADE,
    original_input_cost BIGINT NOT NULL CHECK (original_input_cost >= 0),
    original_output_cost BIGINT NOT NULL CHECK (original_output_cost >= 0),
    original_total_cost BIGINT NOT NULL CHECK (original_total_cost >= 0),
    net_input_cost BIGINT NOT NULL CHECK (net_input_cost >= 0),
    net_output_cost BIGINT NOT NULL CHECK (net_output_cost >= 0),
    original_funded_amount BIGINT,
    original_unfunded_amount BIGINT,
    original_billing_details JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (rule_id, usage_id),
    UNIQUE (usage_id),
    CHECK (net_input_cost <= original_input_cost AND net_output_cost <= original_output_cost),
    CHECK (original_input_cost + original_output_cost = original_total_cost)
);

CREATE TABLE usage_credit_allocation_reversals (
    allocation_id UUID PRIMARY KEY REFERENCES usage_credit_allocations(id) ON DELETE CASCADE,
    rule_id UUID NOT NULL REFERENCES organization_usage_discounts(id) ON DELETE CASCADE,
    amount BIGINT NOT NULL CHECK (amount > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE FUNCTION check_usage_credit_allocation_reversal() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM usage_credit_allocations a
        JOIN organization_usage_discounts d ON d.organization_id = a.organization_id
        WHERE a.id = NEW.allocation_id AND d.id = NEW.rule_id
          AND a.inference_usage_id IS NOT NULL AND a.allocation_phase = 'posting'
          AND NEW.amount <= a.amount
    ) THEN
        RAISE EXCEPTION 'Invalid usage allocation reversal';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER usage_credit_allocation_reversal_check
    BEFORE INSERT OR UPDATE ON usage_credit_allocation_reversals
    FOR EACH ROW EXECUTE FUNCTION check_usage_credit_allocation_reversal();

CREATE VIEW effective_usage_credit_allocations AS
    SELECT a.id, a.organization_id, a.inference_usage_id, a.service_usage_id,
           a.credit_type, a.amount - COALESCE(r.amount, 0) AS amount,
           a.organization_limit_id, a.source, a.policy_version, a.allocation_phase,
           a.priority_position, a.created_at
    FROM usage_credit_allocations a
    LEFT JOIN usage_credit_allocation_reversals r ON r.allocation_id = a.id
    WHERE a.amount > COALESCE(r.amount, 0);

COMMENT ON TABLE usage_discount_adjustments IS 'Append-only before images of historical inference discount corrections.';
COMMENT ON TABLE usage_credit_allocation_reversals IS 'Append-only credit notes; original posting allocations are never rewritten.';
