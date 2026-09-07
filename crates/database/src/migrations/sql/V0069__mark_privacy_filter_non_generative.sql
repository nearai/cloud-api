-- `openai/privacy-filter` is a token-classification (PII detection) model, not a
-- generative chat model. Its only real endpoints are /v1/privacy/classify and
-- /v1/privacy/redact; a /v1/chat/completions request against it returns an
-- upstream 404. It was nonetheless seeded with output_modalities = {'text'}, so
-- in GET /v1/models (and GET /v1/model/list) it was indistinguishable from an
-- ordinary chat model — clients had no way to tell it is not a completion model
-- (issue #615).
--
-- This migration TAGS the model with its true modality; it does NOT hide it. The
-- catalog already encodes model kind in output_modalities, and both listing
-- endpoints surface that field to clients (embedding models report
-- {'embedding'}, image models report {'image'}). The privacy filter keeps
-- appearing in the catalog exactly like an image or embedding model; correcting
-- its OUTPUT modality to 'classification' lets a client distinguish it from a
-- chat model instead of it masquerading as {'text'}. The input modality stays
-- {'text'} — it still consumes text. Every path (privacy classify/redact, direct
-- model lookup, admin) is unaffected.
--
-- Idempotent and operator-respecting: it only rewrites the row while it is still
-- on the wrong {'text'} label, mirroring how V0061 repaired a mislabeled catalog
-- row in place. It is a no-op on databases where the row does not exist (e.g.
-- fresh/test databases that seed the model after migrations run). The correction
-- is mirrored into the currently-open model_history snapshot so the audit trail
-- stays faithful, exactly as the app write path does.
-- Step 1: relabel the model row. output_modalities is a JSONB array of strings
-- (V0043), stored as e.g. '["text"]'; set it to the JSON array
-- '["classification"]'. Idempotent — skips the row once it is already fixed.
UPDATE models
SET
    output_modalities = '["classification"]'::jsonb,
    updated_at = NOW()
WHERE model_name = 'openai/privacy-filter'
  AND output_modalities IS DISTINCT FROM '["classification"]'::jsonb;

-- Step 2: correct the currently-open history snapshot in place (a metadata fix,
-- not a new state transition). Kept independent of step 1 — joined to `models`
-- by name — so a prior manual patch to `models` still repairs the open snapshot.
UPDATE model_history mh
SET output_modalities = '["classification"]'::jsonb
FROM models m
WHERE m.model_name = 'openai/privacy-filter'
  AND mh.model_id = m.id
  AND mh.effective_until IS NULL
  AND mh.output_modalities IS DISTINCT FROM '["classification"]'::jsonb;
