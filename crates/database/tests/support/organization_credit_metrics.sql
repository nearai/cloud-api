CREATE TEMP TABLE organizations (id UUID PRIMARY KEY, name TEXT NOT NULL);
CREATE TEMP TABLE workspaces (id UUID PRIMARY KEY, organization_id UUID, name TEXT);
CREATE TEMP TABLE api_keys (id UUID PRIMARY KEY, workspace_id UUID, name TEXT);
CREATE TEMP TABLE organization_usage_log (
    id UUID PRIMARY KEY, organization_id UUID, workspace_id UUID, api_key_id UUID,
    model_name TEXT, created_at TIMESTAMPTZ, input_tokens INTEGER,
    output_tokens INTEGER, cache_read_tokens INTEGER, ttft_ms INTEGER,
    avg_itl_ms DOUBLE PRECISION
);
CREATE TEMP TABLE usage_credit_allocations (
    id BIGINT PRIMARY KEY, organization_id UUID, inference_usage_id UUID,
    credit_type TEXT, amount BIGINT
);
CREATE TEMP TABLE usage_credit_allocation_reversals (
    allocation_id BIGINT PRIMARY KEY, amount BIGINT
);
CREATE TEMP VIEW effective_usage_credit_allocations AS
    SELECT a.organization_id, a.inference_usage_id, a.credit_type,
           a.amount - COALESCE(r.amount, 0) AS amount
    FROM usage_credit_allocations a
    LEFT JOIN usage_credit_allocation_reversals r ON r.allocation_id = a.id
    WHERE a.amount > COALESCE(r.amount, 0);

INSERT INTO organizations VALUES ('00000000-0000-0000-0000-000000000001', 'Credit metrics');
INSERT INTO workspaces
    SELECT lpad(i::text, 32, '0')::uuid,
           '00000000-0000-0000-0000-000000000001', 'workspace-' || i
    FROM generate_series(1, 3) i;
INSERT INTO api_keys SELECT id, id, name FROM workspaces;

-- First row is exactly at start; rows 4 and 5 must not enter the range.
-- Row 6 belongs to a different organization; row 7 has no allocations.
INSERT INTO organization_usage_log
    SELECT lpad(i::text, 32, '0')::uuid,
           lpad(CASE WHEN i = 6 THEN '2' ELSE '1' END, 32, '0')::uuid,
           lpad(CASE WHEN i = 3 THEN '2' ELSE '1' END, 32, '0')::uuid,
           lpad(CASE WHEN i = 3 THEN '2' ELSE '1' END, 32, '0')::uuid,
           'model-a',
           CASE i
               WHEN 1 THEN '2026-10-02 16:00:00+00'::timestamptz
               WHEN 4 THEN '2026-11-02 12:02:00+00'::timestamptz
               WHEN 5 THEN '2026-10-02 15:59:59+00'::timestamptz
               ELSE '2026-10-03 00:00:00+00'::timestamptz
           END,
           10, 4, 3, CASE WHEN i = 3 THEN NULL ELSE 5000 END,
           CASE WHEN i = 3 THEN NULL ELSE 2.5 END
    FROM generate_series(1, 7) i;

INSERT INTO usage_credit_allocations
    SELECT i, organization_id, id, 'postpay', 4000000000
    FROM organization_usage_log
    JOIN generate_series(1, 6) i ON id = lpad(i::text, 32, '0')::uuid;
-- A second allocation on the same request (e.g. late overage settlement).
INSERT INTO usage_credit_allocations VALUES
    (8, '00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000001', 'postpay', 1000000000),
    (9, '00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000001', 'grant', 2000000000),
    (10, '00000000-0000-0000-0000-000000000001', NULL, 'postpay', 9000000000);
INSERT INTO usage_credit_allocation_reversals VALUES (1, 1000000000), (2, 4000000000);
