-- Change chat_signatures table to use composite unique key (chat_id, signing_algo)
-- This allows storing multiple signatures (ECDSA and ED25519) for the same chat_id

-- Drop the existing unique constraint on chat_id
ALTER TABLE chat_signatures DROP CONSTRAINT IF EXISTS chat_signatures_chat_id_key;

-- Add composite unique constraint on (chat_id, signing_algo)
ALTER TABLE chat_signatures ADD UNIQUE (chat_id, signing_algo);
