-- Data behind the `Deprecation` / `Sunset` / `Link` response headers sent on
-- inference responses for a model with a planned deprecation (RFC 9745,
-- RFC 8594).
--
-- `deprecation_date` (V0053) follows the OpenRouter provider spec: it is the
-- date the model goes away, which is what the `Sunset` header carries. The
-- `Deprecation` header carries the date from which use is discouraged, i.e.
-- when the deprecation was announced, and that was not stored.

-- When the planned deprecation was announced. Stamped by the write path when
-- `deprecation_date` goes from NULL to set, cleared together with it.
-- NULL = no planned deprecation.
ALTER TABLE models ADD COLUMN deprecation_announced_at TIMESTAMPTZ;

-- Canonical name of the recommended replacement model. Until now it only
-- lived in `model_deprecation_email_deliveries` and in the free-text
-- `change_reason`. Cleared together with `deprecation_date`. NULL = none.
ALTER TABLE models ADD COLUMN successor_model_name VARCHAR(500);

-- Mirror onto model_history so admin edits are audited (V0053 precedent).
ALTER TABLE model_history ADD COLUMN deprecation_announced_at TIMESTAMPTZ;
ALTER TABLE model_history ADD COLUMN successor_model_name VARCHAR(500);

-- Models that already carry a planned deprecation: the closest record of the
-- announcement is the first history row that has the date set.
UPDATE models m
SET deprecation_announced_at = COALESCE(
    (
        SELECT MIN(h.effective_from)
        FROM model_history h
        WHERE h.model_id = m.id
          AND h.deprecation_date IS NOT NULL
    ),
    m.updated_at
)
WHERE m.deprecation_date IS NOT NULL;
