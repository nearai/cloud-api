-- Contact email for users whose login identity is not a deliverable address.
-- NEAR wallet logins are stored as `{account_id}@near`, and `users.email` is
-- rewritten from the provider identity on every login, so the verified address
-- lives in its own columns. Login never writes them.
--
-- The new columns are nullable with no default, so replicas still running the
-- previous version keep working during a rolling deployment.
-- Fail fast rather than queue login writes behind this DDL if another
-- transaction holds the table.
SET LOCAL lock_timeout = '5s';

ALTER TABLE users
    ADD COLUMN contact_email VARCHAR(255),
    ADD COLUMN contact_email_verified_at TIMESTAMPTZ,
    ADD COLUMN email_opt_in_at TIMESTAMPTZ;

-- A verified contact address belongs to one user, compared case-insensitively.
-- Empty when created: no row has a verified address yet.
CREATE UNIQUE INDEX idx_users_contact_email_verified
    ON users (lower(contact_email))
    WHERE contact_email_verified_at IS NOT NULL;

-- One row per verification email sent. Only the SHA-256 hash of the token is
-- stored, never the token. Rows are also what the send rate limits count.
-- `email` is the address as the user typed it. `email_limit_key` is its
-- canonical form, computed by the service, which the per-address limit counts
-- by so that variants reaching one mailbox count as one address.
CREATE TABLE user_contact_email_verifications (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email VARCHAR(255) NOT NULL,
    email_limit_key VARCHAR(255) NOT NULL,
    token_hash VARCHAR(64) NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Per-user and per-address send counts, and the purge of old rows.
CREATE INDEX idx_user_contact_email_verifications_user
    ON user_contact_email_verifications (user_id, created_at);
CREATE INDEX idx_user_contact_email_verifications_limit_key
    ON user_contact_email_verifications (email_limit_key, created_at);
CREATE INDEX idx_user_contact_email_verifications_created_at
    ON user_contact_email_verifications (created_at);
