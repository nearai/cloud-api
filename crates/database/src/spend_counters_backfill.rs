use crate::repositories::credit_allocation::lock_organization_accounting;
use crate::DbPool;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::time::Duration;
use tokio_postgres::{IsolationLevel, Transaction};
use uuid::Uuid;

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
// ponytail: each apply statement is capped at 5s; larger units require a resumable protocol.
const APPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyCorrection {
    api_key_id: Uuid,
    inference_spent: i64,
    service_spent: i64,
}

/// A repeatable-read snapshot of one organization's raw-minus-counted correction.
///
/// The fields are intentionally private: callers can only obtain corrections
/// from the database snapshot and apply them through the guarded method below.
#[derive(Debug)]
pub struct PreparedSpendBackfill {
    pool: DbPool,
    organization_id: Uuid,
    /// Readiness marker seen by the snapshot; apply only commits if it is unchanged.
    snapshot_ready_at: Option<DateTime<Utc>>,
    key_corrections: Vec<KeyCorrection>,
    inference_spent: i64,
    service_spent: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendBackfillOutcome {
    Applied { key_count: usize },
    AlreadyComplete,
}

/// How far an organization's counters trail its raw history (nano-USD).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpendDrift {
    pub inference_spent: i64,
    pub service_spent: i64,
    pub key_count: usize,
}

impl SpendDrift {
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

impl PreparedSpendBackfill {
    /// Capture one incomplete organization's raw/counter delta without holding
    /// its accounting lock. Returns `None` if the organization is already ready.
    pub async fn prepare(
        pool: &DbPool,
        organization_id: Uuid,
        statement_timeout: Duration,
    ) -> Result<Option<Self>> {
        Self::snapshot(pool, organization_id, statement_timeout, false).await
    }

    /// Like `prepare`, but also snapshots ready organizations so drift left by a
    /// writer that skipped the counters can be measured and repaired.
    pub async fn prepare_including_ready(
        pool: &DbPool,
        organization_id: Uuid,
        statement_timeout: Duration,
    ) -> Result<Self> {
        Self::snapshot(pool, organization_id, statement_timeout, true)
            .await?
            .context("snapshot including ready organizations always returns a correction")
    }

    /// Whether applying would change anything: an incomplete organization still
    /// needs its readiness marker even when its counters already match.
    pub fn needs_apply(&self) -> bool {
        self.snapshot_ready_at.is_none() || !self.drift().is_zero()
    }

    /// The correction this snapshot would apply.
    pub fn drift(&self) -> SpendDrift {
        SpendDrift {
            inference_spent: self.inference_spent,
            service_spent: self.service_spent,
            key_count: self.key_corrections.len(),
        }
    }

    async fn snapshot(
        pool: &DbPool,
        organization_id: Uuid,
        statement_timeout: Duration,
        include_ready: bool,
    ) -> Result<Option<Self>> {
        validate_timeout(statement_timeout)?;
        let mut client = pool
            .get()
            .await
            .context("acquiring database connection for spend snapshot")?;
        let transaction = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .context("starting repeatable-read spend snapshot")?;
        set_timeout(&transaction, "statement_timeout", statement_timeout).await?;

        let balance = transaction
            .query_opt(
                "SELECT spend_counters_ready_at, inference_spent, service_spent
                 FROM organization_balance WHERE organization_id = $1",
                &[&organization_id],
            )
            .await
            .context("reading organization spend readiness")?;
        let Some(balance) = balance else {
            if transaction
                .query_opt(
                    "SELECT id FROM organizations WHERE id = $1",
                    &[&organization_id],
                )
                .await
                .context("checking organization existence")?
                .is_some()
            {
                bail!("organization {organization_id} has no organization_balance row");
            }
            bail!("organization {organization_id} does not exist");
        };
        let snapshot_ready_at: Option<DateTime<Utc>> = balance.get("spend_counters_ready_at");
        if snapshot_ready_at.is_some() && !include_ready {
            transaction
                .commit()
                .await
                .context("committing already-complete spend snapshot")?;
            return Ok(None);
        }
        let counted_inference: i64 = balance.get("inference_spent");
        let counted_service: i64 = balance.get("service_spent");

        let rows = transaction
            .query(
                r#"
                WITH raw_by_key AS (
                    SELECT api_key_id,
                           SUM(total_cost) AS inference_spent,
                           0::NUMERIC AS service_spent
                    FROM organization_usage_log
                    WHERE organization_id = $1
                    GROUP BY api_key_id
                    UNION ALL
                    SELECT api_key_id,
                           0::NUMERIC AS inference_spent,
                           SUM(total_cost) AS service_spent
                    FROM organization_service_usage_log
                    WHERE organization_id = $1
                    GROUP BY api_key_id
                ),
                raw AS (
                    SELECT api_key_id,
                           SUM(inference_spent) AS inference_spent,
                           SUM(service_spent) AS service_spent
                    FROM raw_by_key
                    GROUP BY api_key_id
                ),
                counted AS (
                    SELECT spend.api_key_id, spend.inference_spent, spend.service_spent
                    FROM api_key_spend AS spend
                    JOIN api_keys AS key ON key.id = spend.api_key_id
                    JOIN workspaces AS workspace ON workspace.id = key.workspace_id
                    WHERE workspace.organization_id = $1
                )
                SELECT COALESCE(raw.api_key_id, counted.api_key_id) AS api_key_id,
                       COALESCE(raw.inference_spent, 0)::TEXT AS raw_inference_spent,
                       COALESCE(raw.service_spent, 0)::TEXT AS raw_service_spent,
                       COALESCE(counted.inference_spent, 0) AS counted_inference_spent,
                       COALESCE(counted.service_spent, 0) AS counted_service_spent
                FROM raw
                FULL OUTER JOIN counted ON counted.api_key_id = raw.api_key_id
                ORDER BY api_key_id
                "#,
                &[&organization_id],
            )
            .await
            .context("reading raw and counted spend by API key")?;

        let mut raw_inference = 0_i64;
        let mut raw_service = 0_i64;
        let mut key_corrections = Vec::with_capacity(rows.len());
        for row in rows {
            let api_key_id: Uuid = row.get("api_key_id");
            let raw_key_inference = parse_total(&row.get::<_, String>("raw_inference_spent"))?;
            let raw_key_service = parse_total(&row.get::<_, String>("raw_service_spent"))?;
            raw_inference = raw_inference
                .checked_add(raw_key_inference)
                .context("raw inference spend exceeds BIGINT")?;
            raw_service = raw_service
                .checked_add(raw_key_service)
                .context("raw service spend exceeds BIGINT")?;
            let inference_correction = checked_correction(
                raw_key_inference,
                row.get("counted_inference_spent"),
                api_key_id,
                "inference",
            )?;
            let service_correction = checked_correction(
                raw_key_service,
                row.get("counted_service_spent"),
                api_key_id,
                "service",
            )?;
            if inference_correction != 0 || service_correction != 0 {
                key_corrections.push(KeyCorrection {
                    api_key_id,
                    inference_spent: inference_correction,
                    service_spent: service_correction,
                });
            }
        }
        let inference_spent = checked_correction(
            raw_inference,
            counted_inference,
            organization_id,
            "organization inference",
        )?;
        let service_spent = checked_correction(
            raw_service,
            counted_service,
            organization_id,
            "organization service",
        )?;

        transaction
            .commit()
            .await
            .context("committing spend snapshot before accounting lock")?;
        Ok(Some(Self {
            pool: pool.clone(),
            organization_id,
            snapshot_ready_at,
            key_corrections,
            inference_spent,
            service_spent,
        }))
    }

    /// Apply the prepared delta under the existing organization accounting lock.
    /// The completion marker and every counter correction commit atomically, and
    /// only if the marker is unchanged since the snapshot: a concurrent run that
    /// already applied this correction moved it, so each correction lands once.
    pub async fn apply(self) -> Result<SpendBackfillOutcome> {
        let mut client = self
            .pool
            .get()
            .await
            .context("acquiring database connection for spend apply")?;
        let transaction = client
            .transaction()
            .await
            .context("starting spend apply transaction")?;
        set_timeout(&transaction, "statement_timeout", APPLY_TIMEOUT).await?;
        set_timeout(&transaction, "lock_timeout", LOCK_TIMEOUT).await?;
        lock_organization_accounting(&transaction, self.organization_id)
            .await
            .map_err(|error| anyhow::anyhow!(error))?;

        let ready_at: Option<DateTime<Utc>> = transaction
            .query_one(
                "SELECT spend_counters_ready_at FROM organization_balance WHERE organization_id = $1",
                &[&self.organization_id],
            )
            .await
            .context("rechecking organization spend readiness")?
            .get(0);
        if ready_at != self.snapshot_ready_at {
            transaction
                .commit()
                .await
                .context("committing already-complete spend apply")?;
            return Ok(SpendBackfillOutcome::AlreadyComplete);
        }

        if !self.key_corrections.is_empty() {
            let key_ids: Vec<Uuid> = self
                .key_corrections
                .iter()
                .map(|correction| correction.api_key_id)
                .collect();
            let inference: Vec<i64> = self
                .key_corrections
                .iter()
                .map(|correction| correction.inference_spent)
                .collect();
            let service: Vec<i64> = self
                .key_corrections
                .iter()
                .map(|correction| correction.service_spent)
                .collect();
            transaction
                .execute(
                    r#"
                    INSERT INTO api_key_spend (
                        api_key_id, inference_spent, service_spent, updated_at
                    )
                    SELECT key_id, inference, service, NOW()
                    FROM UNNEST($1::UUID[], $2::BIGINT[], $3::BIGINT[])
                        AS correction(key_id, inference, service)
                    ON CONFLICT (api_key_id) DO UPDATE SET
                        inference_spent = api_key_spend.inference_spent + EXCLUDED.inference_spent,
                        service_spent = api_key_spend.service_spent + EXCLUDED.service_spent,
                        updated_at = NOW()
                    "#,
                    &[&key_ids, &inference, &service],
                )
                .await
                .context("applying API-key spend corrections")?;
        }
        let updated = transaction
            .execute(
                r#"
                UPDATE organization_balance
                SET inference_spent = inference_spent + $2,
                    service_spent = service_spent + $3,
                    spend_counters_ready_at = NOW(),
                    updated_at = NOW()
                WHERE organization_id = $1
                  AND spend_counters_ready_at IS NOT DISTINCT FROM $4
                "#,
                &[
                    &self.organization_id,
                    &self.inference_spent,
                    &self.service_spent,
                    &self.snapshot_ready_at,
                ],
            )
            .await
            .context("marking organization spend counters ready")?;
        if updated != 1 {
            bail!(
                "organization {} disappeared before spend readiness update",
                self.organization_id
            );
        }
        let key_count = self.key_corrections.len();
        transaction
            .commit()
            .await
            .context("committing spend counter corrections")?;
        Ok(SpendBackfillOutcome::Applied { key_count })
    }
}

/// Organizations whose split counters do not yet include their raw history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendCounterReadiness {
    pub missing_balances: i64,
    pub incomplete: i64,
}

impl SpendCounterReadiness {
    pub fn is_ready(&self) -> bool {
        self.missing_balances == 0 && self.incomplete == 0
    }
}

/// Fail unless every organization, including inactive ones, has a balance row
/// with a completed historical spend snapshot.
pub async fn ensure_spend_counters_ready(pool: &DbPool) -> Result<()> {
    let readiness = spend_counter_readiness(pool).await?;
    if !readiness.is_ready() {
        bail!(
            "spend counters are incomplete: {} organizations lack balances, {} remain unreconciled",
            readiness.missing_balances,
            readiness.incomplete
        );
    }
    Ok(())
}

pub async fn spend_counter_readiness(pool: &DbPool) -> Result<SpendCounterReadiness> {
    let client = pool
        .get()
        .await
        .context("acquiring database connection for spend readiness check")?;
    let row = client
        .query_one(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE balance.organization_id IS NULL)::BIGINT,
                COUNT(*) FILTER (
                    WHERE balance.organization_id IS NOT NULL
                      AND balance.spend_counters_ready_at IS NULL
                )::BIGINT
            FROM organizations AS organization
            LEFT JOIN organization_balance AS balance
              ON balance.organization_id = organization.id
            "#,
            &[],
        )
        .await
        .context("checking spend counter readiness")?;
    Ok(SpendCounterReadiness {
        missing_balances: row.get(0),
        incomplete: row.get(1),
    })
}

pub async fn incomplete_organizations(
    pool: &DbPool,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>> {
    organizations(pool, after, limit, false).await
}

/// Every organization in keyset order, ready or not, for drift verification.
pub async fn all_organizations(
    pool: &DbPool,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>> {
    organizations(pool, after, limit, true).await
}

async fn organizations(
    pool: &DbPool,
    after: Option<Uuid>,
    limit: i64,
    include_ready: bool,
) -> Result<Vec<Uuid>> {
    let client = pool
        .get()
        .await
        .context("acquiring database connection for spend organization list")?;
    let rows = client
        .query(
            r#"
            SELECT organization.id
            FROM organizations AS organization
            LEFT JOIN organization_balance AS balance
              ON balance.organization_id = organization.id
            WHERE ($1::UUID IS NULL OR organization.id > $1)
              AND ($3 OR balance.organization_id IS NULL OR balance.spend_counters_ready_at IS NULL)
            ORDER BY organization.id
            LIMIT $2
            "#,
            &[&after, &limit, &include_ready],
        )
        .await
        .context("listing incomplete spend organizations")?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}

async fn set_timeout(
    transaction: &Transaction<'_>,
    setting: &str,
    timeout: Duration,
) -> Result<()> {
    let value = format!("{}ms", timeout.as_millis());
    transaction
        .query_one("SELECT set_config($1, $2, true)", &[&setting, &value])
        .await
        .with_context(|| format!("setting {setting}"))?;
    Ok(())
}

fn validate_timeout(timeout: Duration) -> Result<()> {
    if timeout.as_millis() == 0 || timeout.as_millis() > i32::MAX as u128 {
        bail!(
            "spend snapshot timeout must be between 1ms and {}ms",
            i32::MAX
        );
    }
    Ok(())
}

fn parse_total(value: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .with_context(|| format!("spend total {value} exceeds BIGINT"))
}

fn checked_correction(raw: i64, counted: i64, id: Uuid, kind: &str) -> Result<i64> {
    let correction = raw
        .checked_sub(counted)
        .with_context(|| format!("{kind} correction overflow for {id}"))?;
    if correction < 0 {
        bail!("{kind} counter exceeds raw history for {id}: raw={raw}, counted={counted}");
    }
    Ok(correction)
}
