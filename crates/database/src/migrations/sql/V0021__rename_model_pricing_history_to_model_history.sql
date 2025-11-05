DO $$
BEGIN
    -- Only rename if the old table exists
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
        WHERE table_name = 'model_pricing_history'
        AND table_schema = 'public'
    ) THEN
        ALTER TABLE model_pricing_history RENAME TO model_history;
    END IF;
END $$;

ALTER TABLE model_history
    ADD COLUMN IF NOT EXISTS model_icon VARCHAR(500),
    ADD COLUMN IF NOT EXISTS verifiable BOOLEAN,
    ADD COLUMN IF NOT EXISTS is_active BOOLEAN,
    ADD COLUMN IF NOT EXISTS model_aliases TEXT[];

CREATE OR REPLACE FUNCTION public.track_model_pricing_change()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    alias_list TEXT[];
BEGIN

    SELECT COALESCE(ARRAY_AGG(alias_name ORDER BY alias_name), ARRAY[]::TEXT[])
    INTO alias_list
    FROM model_aliases
    WHERE canonical_model_id = NEW.id
    AND is_active = TRUE;

    UPDATE model_history
    SET effective_until = NOW()
    WHERE model_id = NEW.id
    AND effective_until IS NULL;

    INSERT INTO model_history (
        model_id,
        input_cost_per_token,
        output_cost_per_token,
        context_length,
        model_display_name,
        model_description,
        model_icon,
        verifiable,
        is_active,
        model_aliases,
        effective_from,
        effective_until,
        changed_by,
        change_reason
    )
    VALUES (
        NEW.id,
        NEW.input_cost_per_token,
        NEW.output_cost_per_token,
        NEW.context_length,
        NEW.model_display_name,
        NEW.model_description,
        NEW.model_icon,
        NEW.verifiable,
        NEW.is_active,
        alias_list,
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
$function$;

CREATE OR REPLACE FUNCTION public.track_model_alias_change()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    curr_model_id UUID;
    alias_list TEXT[];
BEGIN

    curr_model_id := COALESCE(NEW.canonical_model_id, OLD.canonical_model_id);

    SELECT COALESCE(ARRAY_AGG(alias_name ORDER BY alias_name), ARRAY[]::TEXT[])
    INTO alias_list
    FROM model_aliases
    WHERE canonical_model_id = curr_model_id
    AND is_active = TRUE;

    UPDATE model_history
    SET effective_until = NOW()
    WHERE model_id = curr_model_id
    AND effective_until IS NULL;

    INSERT INTO model_history (
        model_id,
        input_cost_per_token,
        output_cost_per_token,
        context_length,
        model_display_name,
        model_description,
        model_icon,
        verifiable,
        is_active,
        model_aliases,
        effective_from,
        effective_until,
        changed_by,
        change_reason
    )
    SELECT
        m.id,
        m.input_cost_per_token,
        m.output_cost_per_token,
        m.context_length,
        m.model_display_name,
        m.model_description,
        m.model_icon,
        m.verifiable,
        m.is_active,
        alias_list,
        NOW(),
        NULL,
        'system',
        CASE TG_OP
            WHEN 'INSERT' THEN 'Alias added'
            WHEN 'UPDATE' THEN 'Alias updated'
            WHEN 'DELETE' THEN 'Alias removed'
        END
    FROM models m
    WHERE m.id = curr_model_id;

    RETURN NULL;
END;
$function$;

DROP TRIGGER IF EXISTS model_pricing_change_trigger ON models;

CREATE TRIGGER model_pricing_change_trigger
AFTER INSERT OR UPDATE ON models
FOR EACH ROW
EXECUTE FUNCTION public.track_model_pricing_change();


DROP TRIGGER IF EXISTS model_alias_change_trigger ON model_aliases;

CREATE TRIGGER model_alias_change_trigger
AFTER INSERT OR UPDATE OR DELETE ON model_aliases
FOR EACH ROW
EXECUTE FUNCTION public.track_model_alias_change();