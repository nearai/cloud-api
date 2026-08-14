-- Persist prompt-cache writes separately from ordinary input and cache reads.
-- The write tokens remain included in input_tokens; input_cost stores the
-- separately-priced result computed by the usage service.
ALTER TABLE organization_usage_log
    ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
