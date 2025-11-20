-- Add user_agent column to admin_access_token table
-- This column stores the User-Agent header from the request that created the token.
-- When validating tokens, the User-Agent must match (if set) for security purposes.
ALTER TABLE admin_access_token
ADD COLUMN user_agent TEXT;
