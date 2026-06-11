-- Scheduled model pricing changes. One row per (model, batch); a single
-- admin confirm request creates N rows sharing batch_id. A background task
-- applies each row when effective_at is reached.
CREATE TABLE scheduled_model_pricing_changes (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    batch_id UUID NOT NULL,
    model_id UUID NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    model_name VARCHAR(500) NOT NULL,
    model_display_name VARCHAR(255) NOT NULL,
    -- New values in nano-dollars (scale 9); NULL = field unchanged.
    new_input_cost_per_token BIGINT,
    new_output_cost_per_token BIGINT,
    new_cache_read_cost_per_token BIGINT,
    new_cost_per_image BIGINT,
    -- Pricing snapshot at confirm time, shown as the "old" price in the
    -- notification email and kept for audit.
    old_input_cost_per_token BIGINT NOT NULL,
    old_output_cost_per_token BIGINT NOT NULL,
    old_cache_read_cost_per_token BIGINT NOT NULL,
    old_cost_per_image BIGINT NOT NULL,
    effective_at TIMESTAMPTZ NOT NULL,
    status VARCHAR(20) NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'applying', 'applied', 'cancelled', 'failed')),
    apply_attempts INT NOT NULL DEFAULT 0,
    applied_at TIMESTAMPTZ,
    last_error TEXT,
    cancelled_at TIMESTAMPTZ,
    cancelled_by_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    cancelled_by_user_email VARCHAR(255),
    created_by_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    created_by_user_email VARCHAR(255),
    change_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (num_nonnulls(new_input_cost_per_token, new_output_cost_per_token,
                        new_cache_read_cost_per_token, new_cost_per_image) >= 1)
);

-- At most one open (pending/applying) change per model: scheduling a second
-- change requires cancelling the announced one first.
CREATE UNIQUE INDEX uq_scheduled_pricing_change_open_per_model
    ON scheduled_model_pricing_changes(model_id)
    WHERE status IN ('pending', 'applying');

CREATE INDEX idx_scheduled_pricing_due
    ON scheduled_model_pricing_changes(effective_at)
    WHERE status = 'pending';

CREATE INDEX idx_scheduled_pricing_batch
    ON scheduled_model_pricing_changes(batch_id);
