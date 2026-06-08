-- Per-model attestation policy: what a model REQUIRES of the providers that
-- may serve it. Supersedes the binary `attestation_supported` flag for routing
-- (wired up in a later phase); this migration only adds the data model.
--
-- Policy values:
--   near_only           - only NEAR AI TEE backends (current behavior for vLLM models)
--   near_or_attested_3p - prefer NEAR AI, may fall back to an attested 3rd party
--   attested_3p_only    - only attested 3rd-party providers
--   non_attested        - any provider, including plaintext 3rd parties
--
-- Routing must never fall back from a more-attested to a less-attested provider
-- than the policy requires; that enforcement lands with the routing changes.

-- The old constraint hard-codes "external == non-attested", which makes an
-- attested 3rd-party provider unstorable. Drop it; the new policy column plus
-- the routing-time, capability-derived tier check replace this invariant.
ALTER TABLE models DROP CONSTRAINT IF EXISTS chk_external_provider_no_attestation;

-- Default to the safe value: a model is treated as non-attested unless it is
-- explicitly assigned a stricter policy.
ALTER TABLE models ADD COLUMN attestation_policy VARCHAR(32) NOT NULL DEFAULT 'non_attested';

-- Backfill existing rows from their provider_type: vLLM models are NEAR TEE
-- backends (near_only); external models stay non_attested.
UPDATE models
SET attestation_policy = CASE WHEN provider_type = 'vllm' THEN 'near_only' ELSE 'non_attested' END;

ALTER TABLE models ADD CONSTRAINT chk_attestation_policy_valid
    CHECK (attestation_policy IN ('near_only', 'near_or_attested_3p', 'attested_3p_only', 'non_attested'));

-- Audit mirror (nullable, like the other model_history columns).
ALTER TABLE model_history ADD COLUMN attestation_policy VARCHAR(32);

COMMENT ON COLUMN models.attestation_policy IS
    'Attestation a model requires of its providers: near_only | near_or_attested_3p | attested_3p_only | non_attested. Authoritative for routing/fallback (never falls back below the required tier).';
