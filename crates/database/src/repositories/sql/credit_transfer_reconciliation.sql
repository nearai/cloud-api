-- Executed by GET /v1/admin/organizations/{org_id}/credit/transfer/reconciliation.
-- One statement provides a consistent MVCC snapshot. It never repairs data.
WITH ledger AS (
    SELECT sender_organization_id AS organization_id, source_credit_type AS credit_type,
           SUM(amount::NUMERIC) AS outgoing, 0::NUMERIC AS incoming
    FROM organization_credit_transfers WHERE sender_organization_id = $1 GROUP BY 1, 2
    UNION ALL
    SELECT recipient_organization_id, 'transfer', 0::NUMERIC, SUM(amount::NUMERIC)
    FROM organization_credit_transfers WHERE recipient_organization_id = $1 GROUP BY 1
), expected AS (
    SELECT organization_id, credit_type, SUM(outgoing) AS outgoing, SUM(incoming) AS incoming
    FROM ledger GROUP BY 1, 2
), projections AS (
    SELECT * FROM organization_credit_transfer_totals WHERE organization_id = $1
), mismatches AS (
    SELECT COALESCE(e.credit_type, p.credit_type) AS credit_type,
           e.outgoing AS ledger_outgoing, p.outgoing AS projection_outgoing,
           e.incoming AS ledger_incoming, p.incoming AS projection_incoming
    FROM expected e FULL JOIN projections p USING (organization_id, credit_type)
    WHERE COALESCE(e.outgoing, 0) <> COALESCE(p.outgoing, 0)
       OR COALESCE(e.incoming, 0) <> COALESCE(p.incoming, 0)
), active_limits AS (
    SELECT credit_type, SUM(spend_limit::NUMERIC) AS gross
    FROM organization_limits_history
    WHERE organization_id = $1 AND effective_until IS NULL GROUP BY credit_type
), backing_deficits AS (
    SELECT e.credit_type, e.outgoing, COALESCE(l.gross, 0) AS gross
    FROM expected e LEFT JOIN active_limits l USING (credit_type)
    WHERE e.outgoing > COALESCE(l.gross, 0)
), transfer_position AS (
    SELECT COALESCE((SELECT incoming FROM expected WHERE credit_type = 'transfer'), 0) AS incoming,
           COALESCE((SELECT gross FROM active_limits WHERE credit_type = 'transfer'), 0) AS gross,
           COALESCE((SELECT amount FROM organization_credit_consumption
                     WHERE organization_id = $1 AND credit_type = 'transfer'), 0) AS consumed
), findings AS (
    SELECT 'projection_mismatch' AS kind, to_jsonb(m) AS details FROM mismatches m
    UNION ALL
    SELECT 'outgoing_exceeds_source', to_jsonb(d) FROM backing_deficits d
    UNION ALL
    SELECT 'incoming_limit_mismatch', to_jsonb(p) FROM transfer_position p WHERE incoming <> gross
    UNION ALL
    SELECT 'consumption_exceeds_incoming', to_jsonb(p) FROM transfer_position p WHERE consumed > incoming
)
SELECT jsonb_build_object(
    'organization_id', $1::UUID,
    'organization_exists', EXISTS (SELECT 1 FROM organizations WHERE id = $1),
    'consistent', NOT EXISTS (SELECT 1 FROM findings),
    'findings', COALESCE((SELECT jsonb_agg(to_jsonb(f) ORDER BY kind) FROM findings f), '[]'::JSONB),
    'checked_at', statement_timestamp()
);
