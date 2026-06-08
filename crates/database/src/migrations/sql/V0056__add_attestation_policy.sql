-- Per-model attestation policy: what a model REQUIRES of the providers that
-- may serve it. This migration only adds the data model as groundwork — no code
-- reads or writes the column yet, and behavior is unchanged.
--
-- Policy values:
--   near_only           - only NEAR AI TEE backends (current behavior for vLLM models)
--   near_or_attested_3p - prefer NEAR AI, may fall back to an attested 3rd party
--   attested_3p_only    - only attested 3rd-party providers
--   non_attested        - any provider, including plaintext 3rd parties
--
-- The column is NULLABLE with NO default on purpose: existing rows are backfilled
-- below, but a row created before the consuming routing code lands is left NULL
-- ("not yet classified") rather than misclassified as a real policy. The phase
-- that adds policy-aware routing will set it on insert, re-backfill any NULLs,
-- and tighten it to NOT NULL.
ALTER TABLE models ADD COLUMN attestation_policy VARCHAR(32);

-- Backfill existing rows from their provider_type (constrained to 'vllm' |
-- 'external'; OpenRouter etc. are stored as 'external'). The models table is
-- small, so a full one-time UPDATE is fine.
UPDATE models
SET attestation_policy = CASE WHEN provider_type = 'vllm' THEN 'near_only' ELSE 'non_attested' END;

ALTER TABLE models ADD CONSTRAINT chk_attestation_policy_valid
    CHECK (attestation_policy IS NULL
        OR attestation_policy IN ('near_only', 'near_or_attested_3p', 'attested_3p_only', 'non_attested'));

-- Audit mirror (nullable, like the other model_history columns), backfilled
-- where the historical provider_type is known (pre-V0042 rows have NULL
-- provider_type and stay NULL — we don't invent a policy for them).
ALTER TABLE model_history ADD COLUMN attestation_policy VARCHAR(32);

UPDATE model_history
SET attestation_policy = CASE provider_type WHEN 'vllm' THEN 'near_only' WHEN 'external' THEN 'non_attested' END
WHERE provider_type IS NOT NULL;

-- NOTE: chk_external_provider_no_attestation is intentionally KEPT here. The
-- runtime still treats models.attestation_supported as authoritative, so an
-- attested 3rd-party row would be inconsistent today. Dropping that constraint
-- (to allow attested 3p), setting attestation_policy on insert, and tightening
-- it to NOT NULL all land with the policy-aware routing phase, which also
-- updates the write paths and the external-provider attestation tests.

COMMENT ON COLUMN models.attestation_policy IS
    'Attestation a model requires of its providers: near_only | near_or_attested_3p | attested_3p_only | non_attested (NULL until the routing phase classifies it). Will be authoritative for routing/fallback (never falls back below the required tier).';
