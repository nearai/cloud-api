-- Qwen/Qwen3.5-122B-A10B reasons by default in production, but its
-- OpenRouter metadata was missing the `reasoning` supported feature. Backfill
-- only if an operator has not already patched the row manually.
WITH updated_model AS (
    UPDATE models
    SET
        supported_features = array_append(supported_features, 'reasoning'),
        updated_at = NOW()
    WHERE model_name = 'Qwen/Qwen3.5-122B-A10B'
      AND NOT ('reasoning' = ANY(supported_features))
    RETURNING id, supported_features
)
-- This corrects the currently-open snapshot in place because the snapshot was
-- missing metadata, rather than recording a new model state transition. If the
-- models row was already patched independently, `updated_model` is empty and
-- history is left unchanged.
UPDATE model_history mh
SET supported_features = updated_model.supported_features
FROM updated_model
WHERE mh.model_id = updated_model.id
  AND mh.effective_until IS NULL;
