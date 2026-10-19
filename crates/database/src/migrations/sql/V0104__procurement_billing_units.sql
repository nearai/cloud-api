-- Expand procurement units without changing customer catalog prices.
ALTER TABLE route_cost_profiles DROP CONSTRAINT route_cost_profiles_billing_unit_check;
ALTER TABLE route_cost_profiles ADD CONSTRAINT route_cost_profiles_billing_unit_check
    CHECK (billing_unit IN ('million_tokens','request','image','audio_second'));
ALTER TABLE route_cost_profiles ADD CONSTRAINT route_cost_profiles_unit_rates_check
    CHECK (billing_unit='million_tokens' OR (output_microunits=0 AND cache_read_microunits IS NULL AND cache_write_microunits IS NULL));
ALTER TABLE inference_attempts ADD COLUMN billable_quantity JSONB
    CHECK (billable_quantity IS NULL OR jsonb_typeof(billable_quantity)='object');
