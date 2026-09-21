ALTER TABLE admin_access_token
    ADD COLUMN permission TEXT NOT NULL DEFAULT 'read_write'
    CONSTRAINT admin_access_token_permission_check
        CHECK (permission IN ('read_only', 'read_write'));
