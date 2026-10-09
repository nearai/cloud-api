-- FORWARD-ONLY. refinery runs with abort_missing=true (refinery-core 0.9.1
-- default): once this migration is applied, a binary whose migrations folder
-- lacks this file fails at startup with MissingVersion. Reverting the code that
-- calls record_organization_usage must KEEP this file; the function is then unused.
CREATE FUNCTION record_organization_usage(
    p_id UUID,
    p_organization_id UUID,
    p_workspace_id UUID,
    p_api_key_id UUID,
    p_model_id UUID,
    p_model_name VARCHAR,
    p_input_tokens INTEGER,
    p_output_tokens INTEGER,
    p_cache_read_tokens INTEGER,
    p_cache_write_tokens INTEGER,
    p_input_cost BIGINT,
    p_output_cost BIGINT,
    p_total_cost BIGINT,
    p_inference_type VARCHAR,
    p_ttft_ms INTEGER,
    p_avg_itl_ms DOUBLE PRECISION,
    p_inference_id UUID,
    p_provider_request_id VARCHAR,
    p_stop_reason VARCHAR,
    p_response_id UUID,
    p_image_count INTEGER,
    p_served_provider_tier TEXT,
    p_served_provider_type TEXT,
    p_served_via_fallback BOOLEAN,
    p_billing_details JSONB,
    p_service_tier TEXT,
    p_context_band TEXT,
    p_priority TEXT[],
    p_policy_version VARCHAR
)
RETURNS TABLE(recorded_usage organization_usage_log, was_inserted BOOLEAN, credit_allocations JSONB)
LANGUAGE plpgsql
SECURITY INVOKER
AS $$
DECLARE
    v_usage organization_usage_log%ROWTYPE;
    v_limit RECORD;
    v_rule_id UUID;
    v_basis_points INTEGER;
    v_recorded_at TIMESTAMPTZ;
    v_input_cost BIGINT := p_input_cost;
    v_output_cost BIGINT := p_output_cost;
    v_total_cost BIGINT := p_total_cost;
    v_billing_details JSONB := p_billing_details;
    v_aggregate_available BIGINT;
    v_legacy_unattributed BIGINT;
    v_remaining BIGINT;
    v_available BIGINT;
    v_amount BIGINT;
    v_position INTEGER := 0;
    v_allocations JSONB := '[]'::JSONB;
BEGIN
    PERFORM 1 FROM organizations WHERE id = p_organization_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'Organization not found: %', p_organization_id USING ERRCODE = 'NAORG';
    END IF;

    v_recorded_at := clock_timestamp();
    SELECT d.id, d.discount_basis_points INTO v_rule_id, v_basis_points
    FROM organization_usage_discounts d WHERE d.organization_id = p_organization_id;
    IF v_rule_id IS NOT NULL AND NOT (
        p_inference_id IS NOT NULL AND EXISTS (
            SELECT 1 FROM organization_usage_log u
            WHERE u.organization_id = p_organization_id AND u.inference_id = p_inference_id
              AND NOT COALESCE(u.billing_details ? 'contract_discount', FALSE)
        )
    ) THEN
        IF p_input_cost::NUMERIC + p_output_cost::NUMERIC <> p_total_cost THEN
            RAISE EXCEPTION 'Usage cost components do not reconcile' USING ERRCODE = '23514';
        END IF;
        IF p_billing_details IS NOT NULL AND jsonb_typeof(p_billing_details) <> 'object' THEN
            RAISE EXCEPTION 'Billing details must be an object' USING ERRCODE = '23514';
        END IF;
        IF COALESCE(p_billing_details ? 'contract_discount', FALSE) THEN
            RAISE EXCEPTION 'Organization discount must only be applied by accounting' USING ERRCODE = '23514';
        END IF;
        IF p_input_cost < 0 OR p_output_cost < 0 THEN
            RAISE EXCEPTION 'amounts must be non-negative before discounting' USING ERRCODE = '23514';
        END IF;
        v_billing_details := COALESCE(p_billing_details, '{}'::JSONB) || jsonb_build_object(
            'contract_discount', jsonb_build_object(
                'rule_id', v_rule_id, 'basis_points', v_basis_points,
                'input_cost', p_input_cost, 'output_cost', p_output_cost, 'total_cost', p_total_cost
            )
        );
        v_input_cost := FLOOR((p_input_cost::NUMERIC * (10000 - v_basis_points) + 5000) / 10000)::BIGINT;
        v_output_cost := FLOOR((p_output_cost::NUMERIC * (10000 - v_basis_points) + 5000) / 10000)::BIGINT;
        v_total_cost := v_input_cost + v_output_cost;
    END IF;

    INSERT INTO organization_usage_log (
        id, organization_id, workspace_id, api_key_id,
        model_id, model_name, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
        total_tokens, input_cost, output_cost, total_cost, inference_type, created_at,
        ttft_ms, avg_itl_ms, inference_id, provider_request_id, stop_reason, response_id,
        image_count, served_provider_tier, served_provider_type, served_via_fallback,
        billing_details, service_tier, context_band
    ) VALUES (
        p_id, p_organization_id, p_workspace_id, p_api_key_id,
        p_model_id, p_model_name, p_input_tokens, p_output_tokens, p_cache_read_tokens,
        p_cache_write_tokens, p_input_tokens + p_output_tokens, v_input_cost, v_output_cost,
        v_total_cost, p_inference_type, v_recorded_at, p_ttft_ms, p_avg_itl_ms,
        p_inference_id, p_provider_request_id, p_stop_reason, p_response_id, p_image_count,
        p_served_provider_tier, p_served_provider_type, p_served_via_fallback,
        v_billing_details, p_service_tier, p_context_band
    )
    ON CONFLICT (organization_id, inference_id) WHERE inference_id IS NOT NULL DO NOTHING
    RETURNING * INTO v_usage;

    IF NOT FOUND THEN
        SELECT * INTO STRICT v_usage FROM organization_usage_log u
        WHERE u.organization_id = p_organization_id AND u.inference_id = p_inference_id;
        IF v_usage.workspace_id IS DISTINCT FROM p_workspace_id
            OR v_usage.api_key_id IS DISTINCT FROM p_api_key_id
            OR v_usage.model_id IS DISTINCT FROM p_model_id
            OR v_usage.model_name IS DISTINCT FROM p_model_name
            OR v_usage.input_tokens IS DISTINCT FROM p_input_tokens
            OR v_usage.output_tokens IS DISTINCT FROM p_output_tokens
            OR v_usage.cache_read_tokens IS DISTINCT FROM p_cache_read_tokens
            OR v_usage.cache_write_tokens IS DISTINCT FROM p_cache_write_tokens
            OR v_usage.input_cost IS DISTINCT FROM v_input_cost
            OR v_usage.output_cost IS DISTINCT FROM v_output_cost
            OR v_usage.total_cost IS DISTINCT FROM v_total_cost
            OR v_usage.inference_type IS DISTINCT FROM p_inference_type
            OR v_usage.image_count IS DISTINCT FROM p_image_count
            OR v_usage.billing_details IS DISTINCT FROM v_billing_details
            OR v_usage.service_tier IS DISTINCT FROM p_service_tier
            OR v_usage.context_band IS DISTINCT FROM p_context_band THEN
            RAISE EXCEPTION 'usage id already exists with different billable data' USING ERRCODE = '23514';
        END IF;
        IF v_usage.funded_amount IS NOT NULL THEN
            SELECT COALESCE(jsonb_agg(jsonb_build_object(
                'type', a.credit_type, 'amount', a.amount, 'source', a.source,
                'organization_limit_id', a.organization_limit_id, 'policy_version', a.policy_version
            ) ORDER BY a.created_at, a.priority_position, a.id), '[]'::JSONB) INTO v_allocations
            FROM effective_usage_credit_allocations a
            WHERE a.inference_usage_id = v_usage.id;
        ELSE
            v_allocations := NULL;
        END IF;
        RETURN QUERY SELECT v_usage, FALSE, v_allocations;
        RETURN;
    END IF;

    SELECT COALESCE(LEAST(SUM(GREATEST(active.spend_limit::NUMERIC -
        COALESCE(consumed.amount, 0), 0)), 9223372036854775807), 0)::BIGINT
    INTO v_aggregate_available
    FROM unnest(p_priority::TEXT[]) WITH ORDINALITY AS wanted(credit_type, position)
    JOIN organization_limits_history active
      ON active.organization_id = p_organization_id
     AND active.credit_type = wanted.credit_type AND active.effective_until IS NULL
    LEFT JOIN organization_credit_consumption consumed
      ON consumed.organization_id = active.organization_id
     AND consumed.credit_type = active.credit_type;
    SELECT COALESCE((SELECT legacy_unattributed_amount FROM organization_balance
        WHERE organization_id = p_organization_id), 0)::BIGINT INTO v_legacy_unattributed;
    v_aggregate_available := GREATEST(v_aggregate_available - v_legacy_unattributed, 0);
    v_remaining := v_total_cost;

    FOR v_limit IN
        SELECT active.id, active.credit_type, active.source, active.spend_limit,
               COALESCE(consumed.amount, 0)::BIGINT AS consumed
        FROM unnest(p_priority::TEXT[]) WITH ORDINALITY AS wanted(credit_type, position)
        JOIN organization_limits_history active
          ON active.organization_id = p_organization_id
         AND active.credit_type = wanted.credit_type AND active.effective_until IS NULL
        LEFT JOIN organization_credit_consumption consumed
          ON consumed.organization_id = active.organization_id
         AND consumed.credit_type = active.credit_type
        ORDER BY wanted.position
    LOOP
        EXIT WHEN v_remaining = 0 OR v_aggregate_available = 0;
        v_available := LEAST(GREATEST(v_limit.spend_limit::NUMERIC - v_limit.consumed, 0), 9223372036854775807)::BIGINT;
        v_amount := LEAST(v_remaining, v_available, v_aggregate_available);
        IF v_amount > 0 THEN
            IF v_position > 32767 THEN
                RAISE EXCEPTION 'credit priority is too long' USING ERRCODE = '23514';
            END IF;
            INSERT INTO usage_credit_allocations (
                organization_id, inference_usage_id, service_usage_id,
                credit_type, amount, organization_limit_id, source,
                policy_version, allocation_phase, priority_position
            ) VALUES (
                p_organization_id, v_usage.id, NULL, v_limit.credit_type,
                v_amount, v_limit.id, v_limit.source, p_policy_version, 'posting', v_position
            );
            INSERT INTO organization_credit_consumption (organization_id, credit_type, amount, updated_at)
            VALUES (p_organization_id, v_limit.credit_type, v_amount, NOW())
            ON CONFLICT (organization_id, credit_type) DO UPDATE SET
                amount = organization_credit_consumption.amount + EXCLUDED.amount,
                updated_at = NOW();
            v_allocations := v_allocations || jsonb_build_array(jsonb_build_object(
                'type', v_limit.credit_type, 'amount', v_amount, 'source', v_limit.source,
                'organization_limit_id', v_limit.id, 'policy_version', p_policy_version
            ));
            v_remaining := v_remaining - v_amount;
            v_aggregate_available := v_aggregate_available - v_amount;
        END IF;
        v_position := v_position + 1;
    END LOOP;

    UPDATE organization_balance SET
        unresolved_unfunded_amount = unresolved_unfunded_amount + v_remaining,
        updated_at = NOW()
    WHERE organization_id = p_organization_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'organization accounting balance is missing' USING ERRCODE = 'NABAL';
    END IF;
    UPDATE organization_usage_log SET funded_amount = v_total_cost - v_remaining,
        unfunded_amount = v_remaining, allocation_policy_version = p_policy_version
    WHERE id = v_usage.id RETURNING * INTO v_usage;
    INSERT INTO organization_balance (
        organization_id, total_spent, last_usage_at, total_requests, total_tokens, updated_at
    ) VALUES (
        p_organization_id, v_total_cost, v_recorded_at, 1,
        (p_input_tokens + p_output_tokens)::BIGINT, v_recorded_at
    ) ON CONFLICT (organization_id) DO UPDATE SET
        total_spent = organization_balance.total_spent + EXCLUDED.total_spent,
        total_requests = organization_balance.total_requests + 1,
        total_tokens = organization_balance.total_tokens + EXCLUDED.total_tokens,
        last_usage_at = EXCLUDED.last_usage_at, updated_at = EXCLUDED.updated_at;
    RETURN QUERY SELECT v_usage, TRUE, v_allocations;
END;
$$;

COMMENT ON FUNCTION record_organization_usage(
    UUID, UUID, UUID, UUID, UUID, VARCHAR, INTEGER, INTEGER, INTEGER, INTEGER,
    BIGINT, BIGINT, BIGINT, VARCHAR, INTEGER, DOUBLE PRECISION, UUID, VARCHAR,
    VARCHAR, UUID, INTEGER, TEXT, TEXT, BOOLEAN, JSONB, TEXT, TEXT, TEXT[], VARCHAR
) IS 'Atomically post an inference usage charge and credit allocation under a short organization lock.';
