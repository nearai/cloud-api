\set ON_ERROR_STOP on
BEGIN;
CREATE SCHEMA default_organization_backfill_test;
SET LOCAL search_path = default_organization_backfill_test;
CREATE TABLE users (id UUID PRIMARY KEY);
CREATE TABLE organizations (id UUID PRIMARY KEY, is_active BOOLEAN NOT NULL DEFAULT true);
CREATE TABLE organization_members (user_id UUID, organization_id UUID, joined_at TIMESTAMPTZ);
INSERT INTO users VALUES ('00000000-0000-0000-0000-000000000001'), ('00000000-0000-0000-0000-000000000002');
INSERT INTO organizations VALUES ('00000000-0000-0000-0000-000000000010', false), ('00000000-0000-0000-0000-000000000020', true);
-- Insert in reverse order to exercise the deterministic UUID tie break, retaining an inactive default.
INSERT INTO organization_members VALUES
('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000020', '2026-01-01'),
('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000010', '2026-01-01');
\ir ../../src/migrations/sql/V0081__persist_default_organization.sql
DO $$ BEGIN
    ASSERT (SELECT default_organization_id = '00000000-0000-0000-0000-000000000010'::uuid AND default_organization_source = 'earliest_retained_membership' FROM users WHERE id = '00000000-0000-0000-0000-000000000001');
    ASSERT (SELECT default_organization_id IS NULL AND default_organization_source = 'unresolved' FROM users WHERE id = '00000000-0000-0000-0000-000000000002');
END $$;
INSERT INTO organization_members VALUES ('00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-000000000020', now());
DO $$ BEGIN
    ASSERT (SELECT default_organization_id IS NULL FROM users WHERE id = '00000000-0000-0000-0000-000000000002');
END $$;
ROLLBACK;
