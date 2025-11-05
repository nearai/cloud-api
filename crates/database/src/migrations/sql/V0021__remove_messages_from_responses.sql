-- Remove input_messages and output_message columns from responses table
-- These should be stored as response_items instead

ALTER TABLE responses DROP COLUMN IF EXISTS input_messages;
ALTER TABLE responses DROP COLUMN IF EXISTS output_message;

