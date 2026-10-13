-- Durable record of GDPR user erasures. Stores ids, timestamps and a one-way email
-- digest, used to find an erasure when the person contacts us again. The digest is
-- unkeyed: anyone holding the database can confirm a guessed email. Accepted for v0.
-- One row per erased user.
CREATE TABLE user_erasure_log (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(id),
    admin_user_id UUID NOT NULL REFERENCES users(id),
    requested_at TIMESTAMPTZ NOT NULL,
    erased_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    erased_organization_count INTEGER NOT NULL,
    email_sha256 BYTEA NOT NULL,
    erased_organization_ids UUID[] NOT NULL,
    retained_organization_ids UUID[] NOT NULL,
    UNIQUE (user_id)
);
-- Not unique: the same email can sign up again and be erased again.
CREATE INDEX idx_user_erasure_log_email_sha256 ON user_erasure_log (email_sha256);
