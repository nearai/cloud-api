-- Seed mock admin user for local development
-- This user is used with mock authentication in development mode
-- ID: 11111111-1111-1111-1111-111111111111 (matches hardcoded mock auth user in MockAuthService)
--
-- IMPORTANT: This seed ensures the mock user ID exists in the database.
-- If you see an error like "duplicate key value violates unique constraint users_email_key",
-- it means another user already has the email 'admin@test.com'.
-- To fix: DELETE FROM users WHERE email = 'admin@test.com' AND id != '11111111-1111-1111-1111-111111111111';

INSERT INTO users (
    id, email, username, display_name,
    auth_provider, provider_user_id, is_active, created_at, updated_at
) VALUES (
    '11111111-1111-1111-1111-111111111111'::uuid,
    'admin@test.com',
    'admin',
    'Test Admin',
    'mock',
    'mock-admin',
    true,
    NOW(),
    NOW()
)
ON CONFLICT (id) DO UPDATE SET
    email = EXCLUDED.email,
    username = EXCLUDED.username,
    display_name = EXCLUDED.display_name,
    auth_provider = EXCLUDED.auth_provider,
    provider_user_id = EXCLUDED.provider_user_id,
    is_active = EXCLUDED.is_active,
    updated_at = NOW();
