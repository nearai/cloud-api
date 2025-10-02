-- V2: Add model pricing and metadata table
-- This migration adds a table to store model pricing information and metadata

-- Models table for storing model pricing and metadata
CREATE TABLE models (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    model_name VARCHAR(500) NOT NULL UNIQUE, -- e.g., "openai/gpt-oss-120b"
    model_display_name VARCHAR(500) NOT NULL, -- e.g., "OpenAI: GPT OSS 120B"
    model_description TEXT NOT NULL,
    model_icon VARCHAR(500), -- URL to model icon
    
    -- Pricing information using decimal representation
    input_cost_amount BIGINT NOT NULL DEFAULT 0, -- Amount in smallest currency unit
    input_cost_scale INT NOT NULL DEFAULT 9, -- Number of decimal places  
    input_cost_currency VARCHAR(10) NOT NULL DEFAULT 'USD',
    
    output_cost_amount BIGINT NOT NULL DEFAULT 0, -- Amount in smallest currency unit
    output_cost_scale INT NOT NULL DEFAULT 9, -- Number of decimal places
    output_cost_currency VARCHAR(10) NOT NULL DEFAULT 'USD',
    
    -- Model metadata
    context_length INTEGER NOT NULL DEFAULT 0,
    verifiable BOOLEAN NOT NULL DEFAULT true,
    
    -- Tracking fields
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Add indexes
CREATE INDEX idx_models_name ON models(model_name);
CREATE INDEX idx_models_active ON models(is_active);
CREATE INDEX idx_models_created ON models(created_at);

-- Model pricing history table for tracking pricing changes over time
CREATE TABLE model_pricing_history (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    model_id UUID NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    
    -- Pricing information snapshot
    input_cost_amount BIGINT NOT NULL,
    input_cost_scale INT NOT NULL,
    input_cost_currency VARCHAR(10) NOT NULL,
    
    output_cost_amount BIGINT NOT NULL,
    output_cost_scale INT NOT NULL,
    output_cost_currency VARCHAR(10) NOT NULL,
    
    -- Model metadata snapshot
    context_length INTEGER NOT NULL,
    model_display_name VARCHAR(500) NOT NULL,
    model_description TEXT NOT NULL,
    
    -- Temporal fields
    effective_from TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    effective_until TIMESTAMPTZ, -- NULL means this is the current pricing
    
    -- Tracking fields
    changed_by VARCHAR(100), -- User or system that made the change
    change_reason TEXT, -- Optional reason for the change
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Add indexes for efficient querying
CREATE INDEX idx_pricing_history_model_id ON model_pricing_history(model_id);
CREATE INDEX idx_pricing_history_effective_from ON model_pricing_history(effective_from);
CREATE INDEX idx_pricing_history_effective_until ON model_pricing_history(effective_until);
CREATE INDEX idx_pricing_history_temporal ON model_pricing_history(model_id, effective_from, effective_until);

-- Function to automatically track pricing changes
CREATE OR REPLACE FUNCTION track_model_pricing_change()
RETURNS TRIGGER AS $$
BEGIN
    -- Close out any existing current pricing (where effective_until IS NULL)
    UPDATE model_pricing_history
    SET effective_until = NOW()
    WHERE model_id = NEW.id 
    AND effective_until IS NULL;
    
    -- Insert new pricing history record
    INSERT INTO model_pricing_history (
        model_id,
        input_cost_amount,
        input_cost_scale,
        input_cost_currency,
        output_cost_amount,
        output_cost_scale,
        output_cost_currency,
        context_length,
        model_display_name,
        model_description,
        effective_from,
        effective_until,
        changed_by,
        change_reason
    ) VALUES (
        NEW.id,
        NEW.input_cost_amount,
        NEW.input_cost_scale,
        NEW.input_cost_currency,
        NEW.output_cost_amount,
        NEW.output_cost_scale,
        NEW.output_cost_currency,
        NEW.context_length,
        NEW.model_display_name,
        NEW.model_description,
        NOW(),
        NULL,
        'system',
        CASE 
            WHEN TG_OP = 'INSERT' THEN 'Initial model creation'
            WHEN TG_OP = 'UPDATE' THEN 'Model pricing or metadata updated'
        END
    );
    
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Trigger to automatically track pricing changes on INSERT and UPDATE
CREATE TRIGGER model_pricing_change_trigger
AFTER INSERT OR UPDATE OF 
    input_cost_amount, input_cost_scale, input_cost_currency,
    output_cost_amount, output_cost_scale, output_cost_currency,
    context_length, model_display_name, model_description
ON models
FOR EACH ROW
EXECUTE FUNCTION track_model_pricing_change();
