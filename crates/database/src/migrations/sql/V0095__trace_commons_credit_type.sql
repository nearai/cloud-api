-- V95: a dedicated credit type for Trace Commons contributor credit.
--
-- Like staking_farm, the type has a single writer that only ever raises its
-- active limit to a caller-supplied cumulative total, so no other limit
-- writer can overwrite it and a retried write is harmless.

ALTER TABLE usage_credit_allocations
    DROP CONSTRAINT usage_credit_allocations_credit_type_check;
ALTER TABLE usage_credit_allocations
    ADD CONSTRAINT usage_credit_allocations_credit_type_check
    CHECK (credit_type IN ('grant', 'postpay', 'staking_farm', 'payment', 'trace_commons'));

ALTER TABLE organization_credit_consumption
    DROP CONSTRAINT organization_credit_consumption_credit_type_check;
ALTER TABLE organization_credit_consumption
    ADD CONSTRAINT organization_credit_consumption_credit_type_check
    CHECK (credit_type IN ('grant', 'postpay', 'staking_farm', 'payment', 'trace_commons'));
