-- Seed mock admin user for local development
-- This user is used with mock authentication in development mode
-- ID: 11111111-1111-1111-1111-111111111111 (matches hardcoded mock auth user)

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
ON CONFLICT (email) DO NOTHING;
