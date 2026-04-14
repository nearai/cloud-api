-- Add constraints and indexes for credit events data integrity
--
-- 1. Prevent same user from claiming multiple codes per event (per-user dedup)
-- 2. Composite index for find_unclaimed_code query performance
-- 3. Index on credit_claims.code_id for lookups

-- Prevent the same user from claiming multiple codes for the same event
CREATE UNIQUE INDEX idx_credit_claims_event_user
    ON credit_claims (credit_event_id, user_id);

-- Composite index for the find_unclaimed_code query pattern:
-- WHERE credit_event_id = $1 AND code = $2 AND is_claimed = false
CREATE INDEX idx_credit_event_codes_event_code
    ON credit_event_codes (credit_event_id, code);

-- Index for claim lookups by code_id
CREATE INDEX idx_credit_claims_code_id
    ON credit_claims (code_id);