-- V96: a dedicated credit type for Trace Commons contributor credit.
--
-- Like staking_farm, the type has a single writer that only ever raises its
-- active limit to a caller-supplied cumulative total, so no other limit
-- writer can overwrite it and a retried write is harmless.
--
-- Only the two ledgers that record spending by type are widened. The transfer
-- tables from V95 (organization_credit_transfer_totals.credit_type and
-- organization_credit_transfers.source_credit_type) are deliberately left as
-- they are: their CHECKs are the database-level guarantee that only payment
-- and staking_farm can fund a transfer, and trace_commons must not.
--
-- record_organization_usage (as replaced in V95) is not redefined. It walks
-- the caller-supplied priority array against organization_credit_capacities
-- and names no credit type, so trace_commons is allocated at whatever
-- position the application's priority gives it. No transfer totals row can
-- exist for trace_commons, so its effective capacity equals its limit.
-- No rows are changed.

ALTER TABLE usage_credit_allocations
    DROP CONSTRAINT usage_credit_allocations_credit_type_check;
ALTER TABLE usage_credit_allocations
    ADD CONSTRAINT usage_credit_allocations_credit_type_check
    CHECK (credit_type IN ('grant', 'postpay', 'staking_farm', 'payment', 'transfer', 'trace_commons'));

ALTER TABLE organization_credit_consumption
    DROP CONSTRAINT organization_credit_consumption_credit_type_check;
ALTER TABLE organization_credit_consumption
    ADD CONSTRAINT organization_credit_consumption_credit_type_check
    CHECK (credit_type IN ('grant', 'postpay', 'staking_farm', 'payment', 'transfer', 'trace_commons'));
