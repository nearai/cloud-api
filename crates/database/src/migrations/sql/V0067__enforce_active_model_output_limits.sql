ALTER TABLE models
    ADD CONSTRAINT chk_active_models_have_positive_max_output_length
    CHECK (NOT is_active OR (max_output_length IS NOT NULL AND max_output_length > 0))
    NOT VALID;
