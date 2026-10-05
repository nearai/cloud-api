ALTER TABLE organizations
    ADD COLUMN request_priority INTEGER NOT NULL DEFAULT 0
    CHECK (request_priority BETWEEN -1000 AND 1000);
