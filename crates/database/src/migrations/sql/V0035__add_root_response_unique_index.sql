-- Ensure there is at most one structural root response ("root_response")
-- per conversation. This protects against races in get_or_create_root where
-- concurrent requests for the same conversation could otherwise both try to
-- insert a root row.
--
-- We use a partial unique index that only applies to rows marked as
-- metadata.root_response = true.

CREATE UNIQUE INDEX IF NOT EXISTS idx_responses_root_response_unique_per_conversation
ON responses(conversation_id)
WHERE metadata->>'root_response' = 'true';


