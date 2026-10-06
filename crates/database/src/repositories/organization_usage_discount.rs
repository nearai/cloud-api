//! Canonical inference discounts and bounded, resumable credit-note corrections.
use crate::models::RecordUsageRequest;
use crate::pool::DbPool;
use crate::repositories::usage_hourly::USAGE_HOURLY_LOCK_KEY;
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use services::common::RepositoryError;
use services::usage::ports::UsageDiscount;
use tokio_postgres::{Row, Transaction};
use uuid::Uuid;

pub const BATCH_SIZE: i64 = 500;

#[derive(Debug, thiserror::Error)]
pub enum DiscountError {
    #[error(
        "discount_basis_points must be between 1 and 10000 and apply_since cannot be in the future"
    )]
    Invalid,
    #[error("Organization not found")]
    NotFound,
    #[error("A different usage discount already exists for this organization")]
    Conflict,
    #[error("Historical usage has unsupported or inconsistent funding; no discount was saved")]
    UnsupportedHistory,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrganizationUsageDiscount {
    #[serde(skip)]
    pub id: Uuid,
    pub discount_basis_points: i32,
    pub apply_since: Option<DateTime<Utc>>,
    pub saved_at: DateTime<Utc>,
    pub status: String,
    pub processed_count: i64,
}

impl OrganizationUsageDiscount {
    fn from_row(row: &Row) -> Self {
        Self {
            id: row.get("id"),
            discount_basis_points: row.get("discount_basis_points"),
            apply_since: row.get("apply_since"),
            saved_at: row.get("saved_at"),
            status: row.get("status"),
            processed_count: row.get("processed_count"),
        }
    }
}

#[derive(Clone)]
pub struct OrganizationUsageDiscountRepository {
    pool: DbPool,
}

impl OrganizationUsageDiscountRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, organization_id: Uuid) -> Result<Option<OrganizationUsageDiscount>> {
        let client = self.pool.get().await?;
        Ok(client
            .query_opt(
                "SELECT * FROM organization_usage_discounts WHERE organization_id = $1",
                &[&organization_id],
            )
            .await?
            .as_ref()
            .map(OrganizationUsageDiscount::from_row))
    }

    pub async fn save(
        &self,
        organization_id: Uuid,
        basis_points: i32,
        apply_since: Option<DateTime<Utc>>,
        created_by: Uuid,
    ) -> Result<OrganizationUsageDiscount> {
        // PostgreSQL timestamps have microsecond precision. Normalize before comparing
        // retry terms so an RFC3339 nanosecond input remains idempotent after storage.
        let apply_since = apply_since.map(|value| {
            DateTime::from_timestamp_micros(value.timestamp_micros()).expect("valid timestamp")
        });
        if !(1..=10_000).contains(&basis_points) {
            return Err(DiscountError::Invalid.into());
        }
        let mut client = self.pool.get().await?;
        // Briefly fence the prefix against a posting that has its timestamp but has
        // not committed yet. Release the lock before the potentially large scan.
        let boundary = client.transaction().await?;
        boundary
            .batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        if boundary
            .query_opt(
                "SELECT id FROM organizations WHERE id=$1 FOR UPDATE",
                &[&organization_id],
            )
            .await?
            .is_none()
        {
            return Err(DiscountError::NotFound.into());
        }
        if let Some(row) = boundary
            .query_opt(
                "SELECT * FROM organization_usage_discounts WHERE organization_id=$1",
                &[&organization_id],
            )
            .await?
        {
            let rule = OrganizationUsageDiscount::from_row(&row);
            if rule.discount_basis_points != basis_points || rule.apply_since != apply_since {
                return Err(DiscountError::Conflict.into());
            }
            return Ok(rule);
        }
        let preflight_at: DateTime<Utc> = boundary
            .query_one("SELECT clock_timestamp()", &[])
            .await?
            .get(0);
        if apply_since.is_some_and(|since| since > preflight_at) {
            return Err(DiscountError::Invalid.into());
        }
        boundary.commit().await?;
        // Immutable fully funded postings cannot later gain settlement entries.
        // Unsupported rows fail before saving a rule, without blocking live charging.
        let preflight = client.transaction().await?;
        preflight
            .batch_execute("SET LOCAL statement_timeout = '120s'")
            .await?;
        let preflight_result = if let Some(since) = apply_since {
            validate_history(&preflight, organization_id, since, preflight_at).await
        } else {
            Ok(())
        };
        preflight.rollback().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        // All posting, credit changes and correction batches take this lock first.
        if tx
            .query_opt(
                "SELECT id FROM organizations WHERE id = $1 FOR UPDATE",
                &[&organization_id],
            )
            .await?
            .is_none()
        {
            return Err(DiscountError::NotFound.into());
        }
        if let Some(row) = tx
            .query_opt(
                "SELECT * FROM organization_usage_discounts WHERE organization_id = $1",
                &[&organization_id],
            )
            .await?
        {
            let rule = OrganizationUsageDiscount::from_row(&row);
            if rule.discount_basis_points != basis_points || rule.apply_since != apply_since {
                return Err(DiscountError::Conflict.into());
            }
            return Ok(rule);
        }
        // A concurrent save wins idempotently, even if its worker changed rows during
        // our preflight scan. Otherwise, fail closed on any historical validation error.
        preflight_result?;
        // Transaction-start now() can precede time spent waiting for the accounting lock.
        let saved_at: DateTime<Utc> = tx.query_one("SELECT clock_timestamp()", &[]).await?.get(0);
        if apply_since.is_some() {
            validate_history(&tx, organization_id, preflight_at, saved_at).await?;
        }
        let status = if apply_since.is_some() {
            "applying"
        } else {
            "active"
        };
        let row = tx
            .query_one(
                r#"INSERT INTO organization_usage_discounts
            (organization_id, discount_basis_points, apply_since, saved_at, created_by, status)
            VALUES ($1,$2,$3,$4,$5,$6) RETURNING *"#,
                &[
                    &organization_id,
                    &basis_points,
                    &apply_since,
                    &saved_at,
                    &created_by,
                    &status,
                ],
            )
            .await?;
        tx.commit().await?;
        Ok(OrganizationUsageDiscount::from_row(&row))
    }

    /// One bounded transaction. A skipped organization is retried on the next worker tick.
    /// Returns true if a batch was committed (including the final active transition).
    pub async fn apply_batch(&self, organization_id: Uuid) -> Result<bool> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '30s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        if tx
            .query_opt(
                "SELECT id FROM organizations WHERE id = $1 FOR UPDATE SKIP LOCKED",
                &[&organization_id],
            )
            .await?
            .is_none()
        {
            return Ok(false);
        }
        let Some(rule_row) = tx.query_opt("SELECT * FROM organization_usage_discounts WHERE organization_id = $1 AND status = 'applying' FOR UPDATE", &[&organization_id]).await? else {
            return Ok(false);
        };
        let rule = OrganizationUsageDiscount::from_row(&rule_row);
        let cursor_at: Option<DateTime<Utc>> = rule_row.get("cursor_created_at");
        let cursor_id: Option<Uuid> = rule_row.get("cursor_usage_id");
        // Serialize cost corrections with aggregate replacement. Do not wait while
        // holding the organization lock if the hourly scheduler is already running.
        let locked: bool = tx
            .query_one(
                "SELECT pg_try_advisory_xact_lock($1)",
                &[&USAGE_HOURLY_LOCK_KEY],
            )
            .await?
            .get(0);
        if !locked {
            return Ok(false);
        }
        let rows = tx
            .query(
                r#"SELECT id, created_at FROM organization_usage_log
            WHERE organization_id = $1 AND created_at >= $2 AND created_at < $3
              AND created_at >= COALESCE($4::TIMESTAMPTZ, $2)
              AND ($4::TIMESTAMPTZ IS NULL OR (created_at, id) > ($4, $5))
            ORDER BY created_at, id LIMIT $6"#,
                &[
                    &organization_id,
                    &rule.apply_since,
                    &rule.saved_at,
                    &cursor_at,
                    &cursor_id,
                    &BATCH_SIZE,
                ],
            )
            .await?;
        if rows.is_empty() {
            // Verification is a consistent read snapshot, outside the accounting lock.
            // New postings remain safe: they atomically preserve the same invariants and
            // their database timestamps cannot enter the frozen historical window.
            tx.commit().await?;
            drop(client);
            self.verify_and_activate(organization_id, rule.id).await?;
            return Ok(true);
        }
        let ids: Vec<Uuid> = rows.iter().map(|row| row.get("id")).collect();
        // NUMERIC intermediates match UsageDiscount's checked i128 half-up arithmetic.
        let inserted = tx.execute(r#"INSERT INTO usage_discount_adjustments
            (rule_id, usage_id, original_input_cost, original_output_cost, original_total_cost,
             net_input_cost, net_output_cost, original_funded_amount, original_unfunded_amount, original_billing_details)
            SELECT $1, id, input_cost, output_cost, total_cost,
                   FLOOR((input_cost::NUMERIC * (10000 - $3::INTEGER) + 5000) / 10000)::BIGINT,
                   FLOOR((output_cost::NUMERIC * (10000 - $3::INTEGER) + 5000) / 10000)::BIGINT,
                   funded_amount, unfunded_amount, billing_details
            FROM organization_usage_log WHERE id = ANY($2)"#,
            &[&rule.id, &ids, &rule.discount_basis_points]).await?;
        ensure!(inserted == ids.len() as u64, "Incomplete discount batch");
        // Reverse each original posting in descending priority, never reclassifying
        // later usage into the newly released capacity. Original entries stay immutable.
        let counters = tx.query_one(r#"WITH portions AS (
                SELECT a.id, a.credit_type, a.amount,
                       j.original_total_cost - j.net_input_cost - j.net_output_cost AS delta,
                       COALESCE(SUM(a.amount) OVER (PARTITION BY a.inference_usage_id
                           ORDER BY a.priority_position DESC, a.id DESC
                           ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING),0) AS preceding
                FROM usage_credit_allocations a
                JOIN usage_discount_adjustments j ON j.usage_id = a.inference_usage_id
                WHERE j.rule_id = $1 AND j.usage_id = ANY($2)
            ), reversed AS (
                INSERT INTO usage_credit_allocation_reversals (allocation_id, rule_id, amount)
                SELECT id, $1, LEAST(amount, delta - preceding)::BIGINT FROM portions
                WHERE delta > preceding RETURNING allocation_id, amount
            ), reductions AS (
                SELECT a.credit_type, SUM(r.amount)::BIGINT AS amount
                FROM reversed r JOIN usage_credit_allocations a ON a.id = r.allocation_id
                GROUP BY a.credit_type
            ), updated AS (
                UPDATE organization_credit_consumption c SET amount = c.amount - r.amount, updated_at = clock_timestamp()
                FROM reductions r WHERE c.organization_id = $3 AND c.credit_type = r.credit_type RETURNING c.credit_type
            ) SELECT (SELECT COUNT(*) FROM reductions) = (SELECT COUNT(*) FROM updated) AS consistent"#,
            &[&rule.id, &ids, &organization_id]).await?;
        ensure!(
            counters.get::<_, bool>("consistent"),
            "Missing credit consumption counter"
        );
        tx.execute(r#"UPDATE organization_usage_log u
            SET input_cost = j.net_input_cost, output_cost = j.net_output_cost,
                total_cost = j.net_input_cost + j.net_output_cost,
                funded_amount = CASE WHEN j.original_funded_amount IS NULL THEN NULL
                    ELSE j.net_input_cost + j.net_output_cost END,
                billing_details = COALESCE(j.original_billing_details, '{}'::JSONB) || jsonb_build_object(
                    'contract_discount', jsonb_build_object('rule_id', j.rule_id,
                        'basis_points', $3::INTEGER, 'input_cost', j.original_input_cost,
                        'output_cost', j.original_output_cost, 'total_cost', j.original_total_cost))
            FROM usage_discount_adjustments j
            WHERE u.id = j.usage_id AND j.rule_id = $1 AND j.usage_id = ANY($2)"#,
            &[&rule.id, &ids, &rule.discount_basis_points]).await?;
        let totals = tx
            .query_one(
                r#"SELECT
            SUM(original_total_cost - net_input_cost - net_output_cost)::BIGINT AS delta,
            COALESCE(SUM(original_total_cost - net_input_cost - net_output_cost)
                FILTER (WHERE original_funded_amount IS NULL), 0)::BIGINT AS legacy
            FROM usage_discount_adjustments WHERE rule_id = $1 AND usage_id = ANY($2)"#,
                &[&rule.id, &ids],
            )
            .await?;
        let delta: i64 = totals.get("delta");
        let legacy: i64 = totals.get("legacy");
        let balances = tx.execute(r#"UPDATE organization_balance
            SET total_spent = total_spent - $2, legacy_unattributed_amount = legacy_unattributed_amount - $3,
                updated_at = clock_timestamp()
            WHERE organization_id = $1 AND total_spent >= $2 AND legacy_unattributed_amount >= $3"#,
            &[&organization_id, &delta, &legacy]).await?;
        ensure!(balances == 1, "Discount balance does not reconcile");
        tx.execute(r#"WITH deltas AS (
                SELECT date_trunc('hour', u.created_at, 'UTC') AS hour, u.organization_id,
                    u.workspace_id, u.api_key_id, u.model_id, u.model_name, u.inference_type,
                    u.served_provider_type, u.served_provider_tier, u.served_via_fallback,
                    SUM(j.original_total_cost - j.net_input_cost - j.net_output_cost)::BIGINT AS delta
                FROM organization_usage_log u JOIN usage_discount_adjustments j ON j.usage_id = u.id
                WHERE j.rule_id = $1 AND j.usage_id = ANY($2) GROUP BY 1,2,3,4,5,6,7,8,9,10
            ) UPDATE usage_hourly h SET total_cost = h.total_cost - d.delta FROM deltas d
              WHERE h.hour = d.hour AND h.organization_id = d.organization_id
                AND h.workspace_id = d.workspace_id AND h.api_key_id = d.api_key_id
                AND h.model_id IS NOT DISTINCT FROM d.model_id AND h.model_name = d.model_name
                AND h.inference_type IS NOT DISTINCT FROM d.inference_type
                AND h.served_provider_type IS NOT DISTINCT FROM d.served_provider_type
                AND h.served_provider_tier IS NOT DISTINCT FROM d.served_provider_tier
                AND h.served_via_fallback IS NOT DISTINCT FROM d.served_via_fallback"#,
            &[&rule.id, &ids]).await?;
        // Check the batch before publishing its cursor: missing counters or funding
        // rows must roll everything back rather than silently produce mixed amounts.
        let consistent: bool = tx.query_one(r#"SELECT NOT EXISTS (
            SELECT 1 FROM organization_usage_log u
            WHERE u.id = ANY($1) AND u.funded_amount IS NOT NULL AND u.funded_amount <>
                (SELECT COALESCE(SUM(a.amount),0) FROM effective_usage_credit_allocations a WHERE a.inference_usage_id=u.id)
            )"#, &[&ids]).await?.get(0);
        ensure!(consistent, "Discount allocation does not reconcile");
        let last = rows.last().expect("nonempty batch");
        let last_at: DateTime<Utc> = last.get("created_at");
        let last_id: Uuid = last.get("id");
        let count = rows.len() as i64;
        tx.execute(
            r#"UPDATE organization_usage_discounts SET processed_count = processed_count + $2,
            cursor_created_at = $3, cursor_usage_id = $4 WHERE id = $1"#,
            &[&rule.id, &count, &last_at, &last_id],
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn verify_and_activate(&self, organization_id: Uuid, rule_id: Uuid) -> Result<()> {
        let mut client = self.pool.get().await?;
        let tx = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        tx.batch_execute("SET LOCAL statement_timeout = '120s'")
            .await?;
        let consistent: bool = tx.query_one(r#"
            WITH costs AS (
                SELECT COALESCE(SUM(total_cost),0)::BIGINT AS total,
                       COALESCE(SUM(total_cost) FILTER (WHERE funded_amount IS NULL),0)::BIGINT AS legacy
                FROM (
                    SELECT total_cost, funded_amount FROM organization_usage_log WHERE organization_id=$1
                    UNION ALL
                    SELECT total_cost, funded_amount FROM organization_service_usage_log WHERE organization_id=$1
                ) usage
            ), credits AS (
                SELECT credit_type, SUM(amount)::BIGINT AS amount FROM effective_usage_credit_allocations
                WHERE organization_id=$1 GROUP BY credit_type
            ), counter_mismatch AS (
                SELECT 1 FROM credits a FULL JOIN
                    (SELECT credit_type, amount FROM organization_credit_consumption WHERE organization_id=$1) c
                    USING (credit_type) WHERE COALESCE(a.amount,0) <> COALESCE(c.amount,0)
            ) SELECT
                COALESCE((SELECT b.total_spent = costs.total AND b.legacy_unattributed_amount = costs.legacy
                    FROM organization_balance b CROSS JOIN costs WHERE b.organization_id=$1),
                    (SELECT total=0 AND legacy=0 FROM costs))
                AND NOT EXISTS (SELECT 1 FROM counter_mismatch)
                AND (SELECT COUNT(*) FROM usage_discount_adjustments WHERE rule_id=$2) =
                    (SELECT processed_count FROM organization_usage_discounts WHERE id=$2)
            "#, &[&organization_id, &rule_id]).await?.get(0);
        ensure!(consistent, "Discount accounting conservation check failed");
        // Compare the actual reporting read path, including missing old aggregate
        // grains and the raw recent/partial-hour edges, against canonical raw costs.
        let parity_sql = crate::repositories::usage_hourly::with_usage_rows(
            "(SELECT apply_since FROM organization_usage_discounts WHERE id=$2)",
            "(SELECT saved_at FROM organization_usage_discounts WHERE id=$2)",
            r#"SELECT
                (SELECT COALESCE(SUM(total_cost),0) FROM usage_rows WHERE organization_id=$1)
                = (SELECT COALESCE(SUM(total_cost),0) FROM organization_usage_log
                    WHERE organization_id=$1
                      AND created_at >= (SELECT apply_since FROM organization_usage_discounts WHERE id=$2)
                      AND created_at < (SELECT saved_at FROM organization_usage_discounts WHERE id=$2))"#,
        );
        let hourly_consistent: bool = tx
            .query_one(&parity_sql, &[&organization_id, &rule_id])
            .await?
            .get(0);
        ensure!(
            hourly_consistent,
            "Discount hourly conservation check failed"
        );
        tx.commit().await?;
        let tx = client.transaction().await?;
        crate::repositories::credit_allocation::lock_organization_accounting(&tx, organization_id)
            .await?;
        tx.execute("UPDATE organization_usage_discounts SET status='active' WHERE id=$1 AND status='applying'", &[&rule_id]).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn pending_organizations(&self) -> Result<Vec<Uuid>> {
        let client = self.pool.get().await?;
        Ok(client.query("SELECT organization_id FROM organization_usage_discounts WHERE status = 'applying' ORDER BY saved_at LIMIT 32", &[]).await?.iter().map(|row| row.get(0)).collect())
    }
}

async fn validate_history(
    tx: &Transaction<'_>,
    organization_id: Uuid,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<()> {
    let invalid: bool = tx.query_one(r#"
        SELECT EXISTS (
            SELECT 1 FROM organization_usage_log u
            LEFT JOIN LATERAL (
                SELECT COALESCE(SUM(a.amount),0)::BIGINT AS amount,
                       BOOL_OR(a.allocation_phase <> 'posting') AS settled,
                       BOOL_OR(c.organization_id IS NULL) AS missing_counter
                FROM usage_credit_allocations a
                LEFT JOIN organization_credit_consumption c
                  ON c.organization_id=a.organization_id AND c.credit_type=a.credit_type
                WHERE a.inference_usage_id=u.id
            ) a ON TRUE
            WHERE u.organization_id=$1 AND u.created_at >= $2 AND u.created_at < $3
              AND (u.input_cost<0 OR u.output_cost<0 OR u.input_cost::NUMERIC+u.output_cost <> u.total_cost
                OR u.billing_details ? 'contract_discount'
                OR COALESCE(a.settled,FALSE) OR COALESCE(a.missing_counter,FALSE)
                OR (u.funded_amount IS NULL AND (u.unfunded_amount IS NOT NULL OR a.amount<>0))
                OR (u.funded_amount IS NOT NULL AND (u.unfunded_amount IS DISTINCT FROM 0::BIGINT
                    OR u.funded_amount<>u.total_cost OR a.amount<>u.funded_amount)))
        )"#, &[&organization_id, &since, &until]).await?.get(0);
    if invalid {
        return Err(DiscountError::UnsupportedHistory.into());
    }
    Ok(())
}

/// Called under the same accounting lock as rule creation. An old inference retry
/// retains its original pricing boundary, even when a rule was saved in between.
pub async fn apply_to_request(
    tx: &Transaction<'_>,
    request: &mut RecordUsageRequest,
) -> Result<(), RepositoryError> {
    let existing = if let Some(inference_id) = request.inference_id {
        tx.query_opt("SELECT billing_details FROM organization_usage_log WHERE organization_id=$1 AND inference_id=$2", &[&request.organization_id, &inference_id]).await.map_err(crate::repositories::utils::map_db_error)?
    } else {
        None
    };
    if existing.as_ref().is_some_and(|row| {
        row.get::<_, Option<serde_json::Value>>(0)
            .is_none_or(|details| details.get("contract_discount").is_none())
    }) {
        return Ok(());
    }
    let Some(row) = tx.query_opt("SELECT id, discount_basis_points FROM organization_usage_discounts WHERE organization_id=$1", &[&request.organization_id]).await.map_err(crate::repositories::utils::map_db_error)? else { return Ok(()); };
    let rule_id: Uuid = row.get("id");
    let basis_points: i32 = row.get("discount_basis_points");
    let discount = UsageDiscount::from_basis_points(basis_points as u16)
        .map_err(|error| RepositoryError::ValidationFailed(error.to_string()))?;
    if request.input_cost.checked_add(request.output_cost) != Some(request.total_cost) {
        return Err(RepositoryError::ValidationFailed(
            "Usage cost components do not reconcile".into(),
        ));
    }
    let mut details = request
        .billing_details
        .take()
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(object) = details.as_object_mut() else {
        return Err(RepositoryError::ValidationFailed(
            "Billing details must be an object".into(),
        ));
    };
    if object.contains_key("contract_discount") {
        return Err(RepositoryError::ValidationFailed(
            "Organization discount must only be applied by accounting".into(),
        ));
    }
    object.insert("contract_discount".into(), serde_json::json!({
        "rule_id": rule_id, "basis_points": basis_points,
        "input_cost": request.input_cost, "output_cost": request.output_cost, "total_cost": request.total_cost
    }));
    request.input_cost = discount
        .apply(request.input_cost)
        .map_err(|error| RepositoryError::ValidationFailed(error.to_string()))?;
    request.output_cost = discount
        .apply(request.output_cost)
        .map_err(|error| RepositoryError::ValidationFailed(error.to_string()))?;
    request.total_cost = request.input_cost + request.output_cost;
    request.billing_details = Some(details);
    Ok(())
}
