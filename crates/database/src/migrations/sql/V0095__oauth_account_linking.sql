-- Keep users.id and the original provider authoritative for account resources.
-- Additional sign-in identities are linked only by an authenticated user.
CREATE TABLE user_oauth_identities (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider VARCHAR(20) NOT NULL CHECK (provider IN ('google', 'github')),
    provider_user_id TEXT NOT NULL,
    email TEXT,
    email_verified BOOLEAN NOT NULL DEFAULT false,
    linked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (provider, provider_user_id),
    UNIQUE (user_id, provider)
);

INSERT INTO user_oauth_identities (user_id, provider, provider_user_id, email, linked_at)
SELECT id, auth_provider, provider_user_id, email, created_at FROM users
WHERE auth_provider IN ('google', 'github');

-- Also cover users created by an older API instance during a rolling deploy.
CREATE FUNCTION register_primary_oauth_identity() RETURNS trigger AS $$
BEGIN
    IF NEW.auth_provider IN ('google', 'github') THEN
        INSERT INTO user_oauth_identities (user_id, provider, provider_user_id, email, linked_at)
        VALUES (NEW.id, NEW.auth_provider, NEW.provider_user_id, NEW.email, NEW.created_at);
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER users_register_primary_oauth_identity
AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION register_primary_oauth_identity();

ALTER TABLE oauth_states
    ADD COLUMN link_user_id UUID REFERENCES users(id) ON DELETE CASCADE,
    ADD COLUMN link_session_id UUID REFERENCES refresh_tokens(id) ON DELETE CASCADE,
    ADD CONSTRAINT oauth_link_context_pair CHECK
        ((link_user_id IS NULL) = (link_session_id IS NULL));

-- An OAuth callback prepares a one-use handoff. The original browser session
-- must confirm it before any identity is attached; callbacks never create sessions.
CREATE TABLE oauth_link_requests (
    code_hash CHAR(64) PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id UUID NOT NULL REFERENCES refresh_tokens(id) ON DELETE CASCADE,
    provider VARCHAR(20) NOT NULL CHECK (provider IN ('google', 'github')),
    provider_user_id TEXT NOT NULL,
    email TEXT NOT NULL,
    email_verified BOOLEAN NOT NULL CHECK (email_verified),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX oauth_link_requests_expiration ON oauth_link_requests(expires_at);
