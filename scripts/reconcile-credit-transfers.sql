-- Read-only. Run after deployment and before enabling each reseller.
-- Expected: zero rows. Compare NUMERIC sums to avoid audit overflow.
WITH ledger AS (
    SELECT sender_organization_id AS organization_id, source_credit_type AS credit_type,
           SUM(amount::NUMERIC) AS outgoing, 0::NUMERIC AS incoming
    FROM organization_credit_transfers GROUP BY 1, 2
    UNION ALL
    SELECT recipient_organization_id, 'transfer', 0::NUMERIC, SUM(amount::NUMERIC)
    FROM organization_credit_transfers GROUP BY 1
), expected AS (
    SELECT organization_id, credit_type, SUM(outgoing) AS outgoing, SUM(incoming) AS incoming
    FROM ledger GROUP BY 1, 2
)
SELECT COALESCE(e.organization_id, p.organization_id) AS organization_id,
       COALESCE(e.credit_type, p.credit_type) AS credit_type,
       e.outgoing AS ledger_outgoing, p.outgoing AS projection_outgoing,
       e.incoming AS ledger_incoming, p.incoming AS projection_incoming
FROM expected e FULL JOIN organization_credit_transfer_totals p USING (organization_id, credit_type)
WHERE COALESCE(e.outgoing, 0) <> COALESCE(p.outgoing, 0)
   OR COALESCE(e.incoming, 0) <> COALESCE(p.incoming, 0);

-- Expected: zero rows, including inactive organizations retained for billing.
SELECT COALESCE(t.organization_id, l.organization_id) AS organization_id,
       t.incoming, l.spend_limit
FROM (SELECT * FROM organization_credit_transfer_totals WHERE credit_type = 'transfer') t
FULL JOIN (SELECT * FROM organization_limits_history
           WHERE credit_type = 'transfer' AND effective_until IS NULL) l
USING (organization_id)
WHERE COALESCE(t.incoming, 0) <> COALESCE(l.spend_limit, 0);

-- Expected: difference = 0. Transfers never appear as new purchases or usage.
SELECT COALESCE(SUM(outgoing::NUMERIC), 0) - COALESCE(SUM(incoming::NUMERIC), 0) AS difference
FROM organization_credit_transfer_totals;
