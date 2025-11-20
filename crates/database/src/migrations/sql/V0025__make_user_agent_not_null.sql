-- Make user_agent NOT NULL in sessions table
-- First, update any existing NULL values to an empty string as a default
UPDATE sessions
SET user_agent = ''
WHERE user_agent IS NULL;

-- Now add the NOT NULL constraint
ALTER TABLE sessions
ALTER COLUMN user_agent SET NOT NULL;

