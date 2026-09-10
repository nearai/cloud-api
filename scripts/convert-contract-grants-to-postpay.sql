-- One-off conversion for contract organizations that were historically modeled
-- as grant credit. This script intentionally requires an explicit, reviewed
-- organization list: grant + postpay is valid for ordinary promotional grants,
-- so the application must not infer which grants are legacy contract ceilings.
--
-- Usage:
--   1. Replace the example row below with the approved organization UUID(s),
--      current active grant amount, and approved postpay ceiling. Amounts are
--      nano-dollars ($1 = 1,000,000,000).
--   2. Run once with the final COMMIT changed to ROLLBACK and review the output.
--   3. Run the reviewed script during the API-first rollout, before enabling
--      postpay in the admin UI. Keep the final COMMIT for the real conversion.
--
-- The table lock makes the validation and conversion atomic with respect to
-- normal limit inserts/updates. Keep the transaction short.

BEGIN;

LOCK TABLE organization_limits_history IN SHARE ROW EXCLUSIVE MODE;

CREATE TEMP TABLE postpay_conversion_targets (
    organization_id UUID PRIMARY KEY,
    expected_grant_limit BIGINT NOT NULL,
    postpay_limit BIGINT NOT NULL
) ON COMMIT DROP;

INSERT INTO postpay_conversion_targets (
    organization_id,
    expected_grant_limit,
    postpay_limit
) VALUES
    ('REPLACE-WITH-APPROVED-ORGANIZATION-UUID'::UUID, 0, 0);

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM postpay_conversion_targets
        WHERE expected_grant_limit <= 0
           OR postpay_limit <= 0
           OR postpay_limit > 1000000000000000000
    ) THEN
        RAISE EXCEPTION 'target limits must match the reviewed grant, and postpay must be in (0, $1B]';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM postpay_conversion_targets t
        LEFT JOIN organizations o
            ON o.id = t.organization_id AND o.is_active = TRUE
        WHERE o.id IS NULL
    ) THEN
        RAISE EXCEPTION 'a target organization is missing or inactive';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM postpay_conversion_targets t
        WHERE (
            SELECT COUNT(*)
            FROM organization_limits_history olh
            WHERE olh.organization_id = t.organization_id
              AND olh.credit_type = 'grant'
              AND olh.effective_until IS NULL
              AND olh.spend_limit = t.expected_grant_limit
        ) <> 1
    ) THEN
        RAISE EXCEPTION 'each target must have exactly one matching active legacy grant';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM postpay_conversion_targets t
        JOIN organization_limits_history olh
          ON olh.organization_id = t.organization_id
         AND olh.credit_type = 'postpay'
         AND olh.effective_until IS NULL
    ) THEN
        RAISE EXCEPTION 'a target already has an active postpay limit';
    END IF;
END
$$;

UPDATE organization_limits_history olh
SET effective_until = transaction_timestamp()
FROM postpay_conversion_targets t
WHERE olh.organization_id = t.organization_id
  AND olh.credit_type = 'grant'
  AND olh.effective_until IS NULL
  AND olh.spend_limit = t.expected_grant_limit;

INSERT INTO organization_limits_history (
    organization_id,
    spend_limit,
    credit_type,
    source,
    currency,
    effective_from,
    changed_by,
    change_reason
)
SELECT
    organization_id,
    postpay_limit,
    'postpay',
    'contract',
    'USD',
    transaction_timestamp(),
    'issue-704-backfill',
    'Convert legacy contract grant to postpay'
FROM postpay_conversion_targets;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM postpay_conversion_targets t
        WHERE EXISTS (
            SELECT 1
            FROM organization_limits_history olh
            WHERE olh.organization_id = t.organization_id
              AND olh.credit_type = 'grant'
              AND olh.effective_until IS NULL
        )
        OR (
            SELECT COUNT(*)
            FROM organization_limits_history olh
            WHERE olh.organization_id = t.organization_id
              AND olh.credit_type = 'postpay'
              AND olh.effective_until IS NULL
              AND olh.spend_limit = t.postpay_limit
        ) <> 1
    ) THEN
        RAISE EXCEPTION 'post-conversion verification failed';
    END IF;
END
$$;

SELECT
    t.organization_id,
    t.expected_grant_limit,
    t.postpay_limit,
    COUNT(*) FILTER (
        WHERE olh.credit_type = 'grant' AND olh.effective_until IS NULL
    ) AS active_grants,
    COUNT(*) FILTER (
        WHERE olh.credit_type = 'postpay' AND olh.effective_until IS NULL
    ) AS active_postpay_rows,
    COALESCE(SUM(olh.spend_limit) FILTER (WHERE olh.effective_until IS NULL), 0)
        AS total_active_limit
FROM postpay_conversion_targets t
LEFT JOIN organization_limits_history olh
    ON olh.organization_id = t.organization_id
GROUP BY t.organization_id, t.expected_grant_limit, t.postpay_limit
ORDER BY t.organization_id;

COMMIT;
