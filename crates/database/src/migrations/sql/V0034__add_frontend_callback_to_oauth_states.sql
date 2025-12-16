-- Add frontend_callback to oauth_states table for dynamic redirect URLs
-- This allows different frontends to specify where they want to be redirected after OAuth
ALTER TABLE oauth_states
ADD COLUMN frontend_callback TEXT;
