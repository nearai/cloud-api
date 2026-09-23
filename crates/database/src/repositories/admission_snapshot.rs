use crate::pool::DbPool;
use crate::repositories::credit_allocation::aggregate_credit_capacity;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use services::usage::admission::{
    AdmissionSnapshotRepository, KeyAdmissionSnapshot, OrganizationAdmissionSnapshot,
};
use services::usage::OrganizationLimit;
use std::time::Duration;
use tokio_postgres::IsolationLevel;
use uuid::Uuid;

/// Primary PostgreSQL reader for the replaceable admission snapshots.
///
/// Both organization and key reads use one repeatable-read transaction so the
/// revision and all accounting fields describe the same committed state.
#[derive(Debug, Clone)]
pub struct PgAdmissionSnapshotRepository {
    pool: DbPool,
    statement_timeout: Duration,
}

impl PgAdmissionSnapshotRepository {
    pub fn new(pool: DbPool, statement_timeout: Duration) -> Self {
        Self {
            pool,
            statement_timeout,
        }
    }
}

#[async_trait]
impl AdmissionSnapshotRepository for PgAdmissionSnapshotRepository {
    async fn load_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<OrganizationAdmissionSnapshot> {
        let mut client = self
            .pool
            .get()
            .await
            .context("failed to get database connection")?;
        let transaction = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .context("failed to start admission snapshot transaction")?;
        transaction
            .query_one(
                "SELECT set_config('statement_timeout', $1, true)",
                &[&format!("{}ms", self.statement_timeout.as_millis())],
            )
            .await
            .context("failed to set admission snapshot statement timeout")?;

        let rows = transaction
            .query(
                r#"
                SELECT o.admission_revision, b.total_spent,
                       b.unresolved_unfunded_amount, b.legacy_unattributed_amount,
                       active.spend_limit, COALESCE(consumed.amount, 0)::BIGINT AS consumed
                FROM organizations o
                LEFT JOIN organization_balance b ON b.organization_id = o.id
                LEFT JOIN organization_limits_history active
                  ON active.organization_id = o.id AND active.effective_until IS NULL
                LEFT JOIN organization_credit_consumption consumed
                  ON consumed.organization_id = o.id AND consumed.credit_type = active.credit_type
                WHERE o.id = $1
                ORDER BY active.credit_type
                "#,
                &[&organization_id],
            )
            .await
            .context("failed to read organization admission snapshot")?;
        transaction
            .commit()
            .await
            .context("failed to commit admission snapshot transaction")?;
        let first = rows
            .first()
            .ok_or_else(|| anyhow!("organization not found: {organization_id}"))?;
        let has_limits = first.get::<_, Option<i64>>("spend_limit").is_some();
        let limit = if has_limits {
            let (spend_limit, available) = aggregate_credit_capacity(
                rows.iter()
                    .map(|row| (row.get("spend_limit"), row.get("consumed"))),
                first
                    .get::<_, Option<i64>>("legacy_unattributed_amount")
                    .unwrap_or(0),
            );
            Some(OrganizationLimit {
                spend_limit,
                available,
                unfunded: first
                    .get::<_, Option<i64>>("unresolved_unfunded_amount")
                    .unwrap_or(0),
            })
        } else {
            None
        };
        Ok(OrganizationAdmissionSnapshot {
            organization_id,
            revision: first.get("admission_revision"),
            total_spent: first.get("total_spent"),
            limit,
        })
    }

    async fn load_key(
        &self,
        organization_id: Uuid,
        api_key_id: Uuid,
    ) -> Result<KeyAdmissionSnapshot> {
        let mut client = self
            .pool
            .get()
            .await
            .context("failed to get database connection")?;
        let transaction = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .context("failed to start admission key snapshot transaction")?;
        transaction
            .query_one(
                "SELECT set_config('statement_timeout', $1, true)",
                &[&format!("{}ms", self.statement_timeout.as_millis())],
            )
            .await
            .context("failed to set admission snapshot statement timeout")?;
        let row = transaction
            .query_opt(
                "SELECT o.admission_revision, o.id AS organization_id, k.id AS api_key_id, k.spend_limit, b.spend_counters_ready_at IS NOT NULL AS counters_ready, COALESCE(s.inference_spent, 0)::BIGINT AS inference_spent FROM api_keys k JOIN workspaces w ON w.id = k.workspace_id JOIN organizations o ON o.id = w.organization_id LEFT JOIN organization_balance b ON b.organization_id = o.id LEFT JOIN api_key_spend s ON s.api_key_id = k.id WHERE o.id = $1 AND k.id = $2",
                &[&organization_id, &api_key_id],
            )
            .await
            .context("failed to read API key admission snapshot")?
            .ok_or_else(|| anyhow!("API key not found for organization: {api_key_id}"))?;
        // ponytail: preserve raw-history fallback until counters are reconciled in every
        // environment; remove with the other readiness fallbacks after that rollout.
        let inference_spent = if row.get::<_, bool>("counters_ready") {
            row.get("inference_spent")
        } else {
            transaction
                .query_one(
                    "SELECT COALESCE(SUM(total_cost), 0)::BIGINT FROM organization_usage_log WHERE api_key_id = $1",
                    &[&api_key_id],
                )
                .await
                .context("failed to read unreconciled API key usage")?
                .get(0)
        };
        let snapshot = KeyAdmissionSnapshot {
            organization_id: row.get("organization_id"),
            api_key_id: row.get("api_key_id"),
            revision: row.get("admission_revision"),
            spend_limit: row.get("spend_limit"),
            inference_spent,
        };
        transaction
            .commit()
            .await
            .context("failed to commit admission key snapshot transaction")?;
        Ok(snapshot)
    }
}
