-- Make user_agent NOT NULL in refresh_tokens table
-- First, update any existing NULL values to an empty string as a default
UPDATE refresh_tokens
SET user_agent = ''
WHERE user_agent IS NULL;

-- Now add the NOT NULL constraint
ALTER TABLE refresh_tokens
ALTER COLUMN user_agent SET NOT NULL;
