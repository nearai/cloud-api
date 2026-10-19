-- Nullable attribution for historical and not-yet-migrated providers.
ALTER TABLE organization_usage_log
    ADD COLUMN served_route_id UUID REFERENCES model_provider_routes(id),
    ADD COLUMN served_deployment_id UUID REFERENCES inference_providers(id),
    ADD CONSTRAINT usage_registry_identity_pair CHECK ((served_route_id IS NULL) = (served_deployment_id IS NULL));

-- Keep the old overload available to older schema-aware writers. Recording the
-- debit, allocations and serving identity is atomic in this overload.
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
    p_policy_version VARCHAR,
    p_served_route_id UUID,
    p_served_deployment_id UUID
)
RETURNS TABLE(recorded_usage organization_usage_log, was_inserted BOOLEAN, credit_allocations JSONB)
LANGUAGE plpgsql SECURITY INVOKER AS $$
DECLARE
    v_result RECORD;
    v_usage organization_usage_log%ROWTYPE;
BEGIN
    IF (p_served_route_id IS NULL) <> (p_served_deployment_id IS NULL) OR
       (p_served_route_id IS NOT NULL AND NOT EXISTS (
           SELECT 1 FROM model_provider_routes r JOIN provider_models pm ON pm.id=r.inventory_id
           WHERE r.id=p_served_route_id AND r.model_id=p_model_id AND pm.provider_id=p_served_deployment_id
       )) THEN
        RAISE EXCEPTION 'Invalid registry serving identity' USING ERRCODE='23514';
    END IF;
    SELECT * INTO v_result FROM record_organization_usage(
        p_id,
        p_organization_id,
        p_workspace_id,
        p_api_key_id,
        p_model_id,
        p_model_name,
        p_input_tokens,
        p_output_tokens,
        p_cache_read_tokens,
        p_cache_write_tokens,
        p_input_cost,
        p_output_cost,
        p_total_cost,
        p_inference_type,
        p_ttft_ms,
        p_avg_itl_ms,
        p_inference_id,
        p_provider_request_id,
        p_stop_reason,
        p_response_id,
        p_image_count,
        p_served_provider_tier,
        p_served_provider_type,
        p_served_via_fallback,
        p_billing_details,
        p_service_tier,
        p_context_band,
        p_priority,
        p_policy_version
    );
    IF v_result.was_inserted THEN
        UPDATE organization_usage_log
        SET served_route_id=p_served_route_id, served_deployment_id=p_served_deployment_id
        WHERE id=(v_result.recorded_usage).id
        RETURNING * INTO v_usage;
    ELSE
        -- Idempotent billing cannot rewrite the original serving identity.
        v_usage := v_result.recorded_usage;
    END IF;
    RETURN QUERY SELECT v_usage, v_result.was_inserted, v_result.credit_allocations;
END;
$$;
