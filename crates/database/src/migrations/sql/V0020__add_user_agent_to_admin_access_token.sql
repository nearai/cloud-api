-- Add user_agent column to admin_access_token table
ALTER TABLE admin_access_token
ADD COLUMN user_agent TEXT;
