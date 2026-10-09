-- Durable record of GDPR user erasures (ids, timestamps and a count only).
-- One row per erased user.
CREATE TABLE user_erasure_log (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(id),
    admin_user_id UUID NOT NULL REFERENCES users(id),
    requested_at TIMESTAMPTZ NOT NULL,
    erased_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    erased_organization_count INTEGER NOT NULL,
    UNIQUE (user_id)
);
