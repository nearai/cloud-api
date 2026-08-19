-- Exact, versioned text pricing profiles. JSONB is validated by the service at
-- every admin write; the database constraints keep the stored shape object-like.
ALTER TABLE models
    ADD COLUMN text_pricing JSONB,
    ADD CONSTRAINT models_text_pricing_object
        CHECK (text_pricing IS NULL OR jsonb_typeof(text_pricing) = 'object');

ALTER TABLE model_history
    ADD COLUMN text_pricing JSONB,
    ADD CONSTRAINT model_history_text_pricing_object
        CHECK (text_pricing IS NULL OR jsonb_typeof(text_pricing) = 'object');

ALTER TABLE scheduled_model_pricing_changes
    ADD COLUMN old_text_pricing JSONB,
    ADD COLUMN new_text_pricing JSONB,
    ADD CONSTRAINT scheduled_old_text_pricing_object
        CHECK (old_text_pricing IS NULL OR jsonb_typeof(old_text_pricing) = 'object'),
    ADD CONSTRAINT scheduled_new_text_pricing_object
        CHECK (new_text_pricing IS NULL OR jsonb_typeof(new_text_pricing) = 'object');

ALTER TABLE organization_usage_log
    ADD COLUMN billing_details JSONB,
    ADD COLUMN service_tier TEXT,
    ADD COLUMN context_band TEXT,
    ADD CONSTRAINT usage_billing_details_object
        CHECK (billing_details IS NULL OR jsonb_typeof(billing_details) = 'object'),
    ADD CONSTRAINT usage_service_tier_known
        CHECK (service_tier IS NULL OR service_tier IN ('default', 'flex', 'priority')),
    ADD CONSTRAINT usage_context_band_known
        CHECK (context_band IS NULL OR context_band IN ('short', 'long'));
