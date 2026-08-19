use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use services::aml::{
    AmlAllowlistEntry, AmlAllowlistPage, AmlReport, AmlReportPage, AmlReportPolicySignals,
    AmlRepository, AmlRiskLevel, NewAmlAllowlistEntry, NewAmlReport,
};
use services::common::RepositoryError;
use tokio_postgres::Row;

#[derive(Debug, Clone)]
pub struct PostgresAmlRepository {
    pool: DbPool,
}

impl PostgresAmlRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn row_to_report(row: Row) -> AmlReport {
        AmlReport {
            id: row.get("id"),
            user_id: row.get("user_id"),
            flow: row.get("flow"),
            provider: row.get("provider"),
            account_id: row.get("account_id"),
            address_type: row.get("address_type"),
            risk_level: db_risk_level(row.get::<_, String>("risk_level").as_str()),
            score: row.get("score"),
            report_id: row.get("report_id"),
            reason: row.get("reason"),
            provider_report_time: row.get("provider_report_time"),
            result_json: row.get("result_json"),
            active: row.get("active"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }

    fn row_to_allowlist_entry(row: Row) -> AmlAllowlistEntry {
        AmlAllowlistEntry {
            account_id: row.get("account_id"),
            address_type: row.get("address_type"),
            reason: row.get("reason"),
            created_by_user_id: row.get("created_by_user_id"),
            created_at: row.get("created_at"),
        }
    }
}

#[async_trait]
impl AmlRepository for PostgresAmlRepository {
    async fn latest_active_report(
        &self,
        account_id: &str,
        address_type: &str,
        provider: &str,
    ) -> Result<Option<AmlReport>> {
        let row = retry_db!("get_latest_active_aml_report", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT *
                    FROM aml_reports
                    WHERE provider = $1
                      AND address_type = $2
                      AND account_id = $3
                      AND active = true
                    ORDER BY created_at DESC
                    LIMIT 1
                    "#,
                    &[&provider, &address_type, &account_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.map(Self::row_to_report))
    }

    async fn latest_fresh_active_report(
        &self,
        account_id: &str,
        address_type: &str,
        provider: &str,
        fresh_after: DateTime<Utc>,
    ) -> Result<Option<AmlReport>> {
        let row = retry_db!("get_fresh_active_aml_report", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT *
                    FROM aml_reports
                    WHERE provider = $1
                      AND address_type = $2
                      AND account_id = $3
                      AND active = true
                      AND created_at >= $4
                    ORDER BY created_at DESC
                    LIMIT 1
                    "#,
                    &[&provider, &address_type, &account_id, &fresh_after],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.map(Self::row_to_report))
    }

    async fn create_report(&self, report: NewAmlReport) -> Result<AmlReport> {
        let risk_level = report.risk_level.as_str();
        let flow = report.flow.as_str();
        let row = retry_db!("create_aml_report", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    INSERT INTO aml_reports (
                        user_id, flow, provider, account_id, address_type, risk_level,
                        score, report_id, reason, provider_report_time, result_json, active
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                    RETURNING *
                    "#,
                    &[
                        &report.user_id,
                        &flow,
                        &report.provider,
                        &report.account_id,
                        &report.address_type,
                        &risk_level,
                        &report.score,
                        &report.report_id,
                        &report.reason,
                        &report.provider_report_time,
                        &report.result_json,
                        &report.active,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(Self::row_to_report(row))
    }

    async fn list_reports(&self, limit: i64, offset: i64) -> Result<AmlReportPage> {
        let (total, rows) = retry_db!("list_aml_reports", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let total = client
                .query_one("SELECT COUNT(*) AS total_count FROM aml_reports", &[])
                .await
                .map_err(map_db_error)?
                .get::<_, i64>("total_count");

            let rows = client
                .query(
                    r#"
                    SELECT
                        id,
                        user_id,
                        flow,
                        provider,
                        account_id,
                        address_type,
                        risk_level,
                        score,
                        report_id,
                        reason,
                        provider_report_time,
                        '{}'::jsonb AS result_json,
                        active,
                        created_at,
                        updated_at
                    FROM aml_reports
                    ORDER BY created_at DESC, id DESC
                    LIMIT $1 OFFSET $2
                    "#,
                    &[&limit, &offset],
                )
                .await
                .map_err(map_db_error)?;

            Ok::<_, RepositoryError>((total, rows))
        })?;

        let reports = rows.into_iter().map(Self::row_to_report).collect();
        Ok(AmlReportPage {
            reports,
            total,
            limit,
            offset,
        })
    }

    async fn set_report_active(
        &self,
        report_id: uuid::Uuid,
        active: bool,
    ) -> Result<Option<AmlReport>> {
        let row = retry_db!("set_aml_report_active", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    UPDATE aml_reports
                    SET active = $2, updated_at = NOW()
                    WHERE id = $1
                    RETURNING *
                    "#,
                    &[&report_id, &active],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.map(Self::row_to_report))
    }

    async fn report_policy_signals(
        &self,
        report_id: uuid::Uuid,
    ) -> Result<Option<AmlReportPolicySignals>> {
        let row = retry_db!("get_aml_report_policy_signals", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT risk_level, score
                    FROM aml_reports
                    WHERE id = $1
                    "#,
                    &[&report_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.map(|row| AmlReportPolicySignals {
            risk_level: db_risk_level(row.get::<_, String>("risk_level").as_str()),
            score: row.get("score"),
        }))
    }

    async fn is_allowlisted(&self, account_id: &str, address_type: &str) -> Result<bool> {
        let row = retry_db!("is_aml_account_allowlisted", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT id
                    FROM aml_allowlisted_accounts
                    WHERE account_id = $1 AND address_type = $2
                    "#,
                    &[&account_id, &address_type],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.is_some())
    }

    async fn list_allowlist_entries(&self, limit: i64, offset: i64) -> Result<AmlAllowlistPage> {
        let (total, rows) = retry_db!("list_aml_allowlist_entries", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let total = client
                .query_one(
                    "SELECT COUNT(*) AS total_count FROM aml_allowlisted_accounts",
                    &[],
                )
                .await
                .map_err(map_db_error)?
                .get::<_, i64>("total_count");

            let rows = client
                .query(
                    r#"
                    SELECT account_id, address_type, reason, created_by_user_id, created_at
                    FROM aml_allowlisted_accounts
                    ORDER BY created_at DESC, account_id ASC
                    LIMIT $1 OFFSET $2
                    "#,
                    &[&limit, &offset],
                )
                .await
                .map_err(map_db_error)?;

            Ok::<_, RepositoryError>((total, rows))
        })?;

        Ok(AmlAllowlistPage {
            entries: rows.into_iter().map(Self::row_to_allowlist_entry).collect(),
            total,
            limit,
            offset,
        })
    }

    async fn upsert_allowlist_entry(
        &self,
        entry: NewAmlAllowlistEntry,
    ) -> Result<AmlAllowlistEntry> {
        let row = retry_db!("upsert_aml_allowlist_entry", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    INSERT INTO aml_allowlisted_accounts (
                        account_id, address_type, reason, created_by_user_id
                    )
                    VALUES ($1, $2, $3, $4)
                    ON CONFLICT (account_id, address_type)
                    DO UPDATE SET
                        reason = COALESCE(EXCLUDED.reason, aml_allowlisted_accounts.reason)
                    RETURNING account_id, address_type, reason, created_by_user_id, created_at
                    "#,
                    &[
                        &entry.account_id,
                        &entry.address_type,
                        &entry.reason,
                        &entry.created_by_user_id,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(Self::row_to_allowlist_entry(row))
    }

    async fn remove_allowlist_entry(&self, account_id: &str, address_type: &str) -> Result<bool> {
        let rows_affected = retry_db!("remove_aml_allowlist_entry", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    r#"
                    DELETE FROM aml_allowlisted_accounts
                    WHERE account_id = $1 AND address_type = $2
                    "#,
                    &[&account_id, &address_type],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }
}

fn db_risk_level(value: &str) -> AmlRiskLevel {
    match value {
        "LOW" => AmlRiskLevel::Low,
        "MEDIUM" => AmlRiskLevel::Medium,
        "HIGH" => AmlRiskLevel::High,
        _ => AmlRiskLevel::Unknown,
    }
}
