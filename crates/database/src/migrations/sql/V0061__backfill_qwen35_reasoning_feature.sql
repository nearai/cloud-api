-- Qwen/Qwen3.5-122B-A10B reasons by default in production, but its
-- OpenRouter metadata was missing the `reasoning` supported feature. Backfill
-- only if an operator has not already patched the row manually.
WITH target_model AS (
    SELECT id
    FROM models
    WHERE model_name = 'Qwen/Qwen3.5-122B-A10B'
),
updated_model AS (
    UPDATE models
    SET
        supported_features = array_append(COALESCE(supported_features, '{}'), 'reasoning'),
        updated_at = NOW()
    WHERE model_name = 'Qwen/Qwen3.5-122B-A10B'
      AND NOT ('reasoning' = ANY(COALESCE(supported_features, '{}')))
    RETURNING id
)
-- This corrects the currently-open snapshot in place because the snapshot was
-- missing metadata, rather than recording a new model state transition. It is
-- independent of the `models` update so a prior manual patch to `models` still
-- repairs the open history snapshot.
UPDATE model_history mh
SET supported_features = array_append(COALESCE(mh.supported_features, '{}'), 'reasoning')
FROM target_model
WHERE mh.model_id = target_model.id
  AND mh.effective_until IS NULL
  AND NOT ('reasoning' = ANY(COALESCE(mh.supported_features, '{}')));
