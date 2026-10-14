//! Trace Commons contributor credit: a dedicated, monotonic credit type.
//!
//! Mirrors the staking-farm writer
//! (`organization_staking_farm_sources.rs::update_staking_farm_limit`): the
//! caller supplies the organization's CUMULATIVE Trace Commons credit, and
//! the active `trace_commons` limit is raised to it under the organization
//! accounting lock. A value at or below the active limit writes nothing, so
//! a retried or reordered request is harmless and the limit never decreases.
//! No other writer touches this credit type: the generic limits PATCH
//! refuses it.
use crate::pool::DbPool;
use crate::repositories::credit_allocation::{
    lock_organization_accounting, settle_unfunded_usage, CreditAllocationPolicy,
};
use anyhow::Result;
use chrono::Utc;
use uuid::Uuid;

pub const CREDIT_TYPE_TRACE_COMMONS: &str = "trace_commons";
pub const CREDIT_SOURCE_TRACE_COMMONS: &str = "trace-commons";
const CHANGED_BY: &str = "trace_commons_sync";
const CHANGE_REASON_PREFIX: &str = "Trace Commons cumulative credit ";

/// Same ceiling the limits PATCH enforces (`services::admin` validation).
pub const MAX_TRACE_COMMONS_NANO_USD: i64 = 1_000_000_000_000_000_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TraceCommonsCreditError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Organization not found or inactive")]
    NotFound,
}

/// The outcome of a cumulative write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceCommonsCreditWrite {
    /// The active `trace_commons` limit after the call.
    pub cumulative_nano_usd: i64,
    /// Whether this call raised it.
    pub applied: bool,
}

/// Validate the caller's statement reference: an opaque, bounded, printable
/// token recorded in `change_reason` so each raise is attributable.
pub fn validate_statement_ref(value: &str) -> Result<(), TraceCommonsCreditError> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(TraceCommonsCreditError::Invalid(
            "statement_ref is malformed",
        ))
    }
}

#[derive(Debug, Clone)]
pub struct TraceCommonsCreditRepository {
    pool: DbPool,
    allocation_policy: CreditAllocationPolicy,
}

impl TraceCommonsCreditRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool,
            allocation_policy: CreditAllocationPolicy::default(),
        }
    }

    pub fn with_accounting_config(pool: DbPool, config: &config::CreditAllocationConfig) -> Self {
        Self {
            pool,
            allocation_policy: CreditAllocationPolicy::from(config),
        }
    }

    /// The active `trace_commons` limit, or 0 when there is none. `None`
    /// when the organization does not exist or is inactive.
    pub async fn get_cumulative(&self, organization_id: Uuid) -> Result<Option<i64>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT (SELECT COALESCE(MAX(spend_limit), 0) FROM organization_limits_history
                          WHERE organization_id = o.id AND credit_type = $2
                            AND effective_until IS NULL) AS cumulative
                   FROM organizations o WHERE o.id = $1 AND o.is_active",
                &[&organization_id, &CREDIT_TYPE_TRACE_COMMONS],
            )
            .await?;
        Ok(row.map(|r| r.get::<_, i64>("cumulative")))
    }

    /// Raise the organization's `trace_commons` limit to `cumulative_nano_usd`
    /// if that is greater than the active limit. Monotonic and idempotent.
    pub async fn set_cumulative(
        &self,
        organization_id: Uuid,
        cumulative_nano_usd: i64,
        statement_ref: &str,
        changed_by_user_id: Option<Uuid>,
    ) -> Result<TraceCommonsCreditWrite> {
        if !(0..=MAX_TRACE_COMMONS_NANO_USD).contains(&cumulative_nano_usd) {
            return Err(TraceCommonsCreditError::Invalid(
                "cumulative_nano_usd must be between 0 and 1e18",
            )
            .into());
        }
        validate_statement_ref(statement_ref)?;

        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        lock_organization_accounting(&tx, organization_id)
            .await
            .map_err(|error| match error {
                services::common::RepositoryError::NotFound(_) => {
                    anyhow::Error::from(TraceCommonsCreditError::NotFound)
                }
                other => other.into(),
            })?;
        // Re-check under the lock, as the staking-farm writer does: an
        // organization deactivated while the request was in flight gets
        // nothing written.
        let active = tx
            .query_opt(
                "SELECT 1 FROM organizations WHERE id = $1 AND is_active",
                &[&organization_id],
            )
            .await?;
        if active.is_none() {
            return Err(TraceCommonsCreditError::NotFound.into());
        }

        let current: i64 = tx
            .query_one(
                "SELECT COALESCE(MAX(spend_limit), 0) FROM organization_limits_history
                  WHERE organization_id = $1 AND credit_type = $2 AND effective_until IS NULL",
                &[&organization_id, &CREDIT_TYPE_TRACE_COMMONS],
            )
            .await?
            .get(0);
        if cumulative_nano_usd <= current {
            tx.commit().await?;
            return Ok(TraceCommonsCreditWrite {
                cumulative_nano_usd: current,
                applied: false,
            });
        }

        let now = Utc::now();
        tx.execute(
            "UPDATE organization_limits_history SET effective_until = $1
              WHERE organization_id = $2 AND credit_type = $3 AND effective_until IS NULL",
            &[&now, &organization_id, &CREDIT_TYPE_TRACE_COMMONS],
        )
        .await?;
        let reason = format!("{CHANGE_REASON_PREFIX}{statement_ref}");
        tx.execute(
            "INSERT INTO organization_limits_history (
                organization_id, spend_limit, credit_type, source, currency,
                effective_from, changed_by, change_reason, changed_by_user_id
             ) VALUES ($1, $2, $3, $4, 'USD', $5, $6, $7, $8)",
            &[
                &organization_id,
                &cumulative_nano_usd,
                &CREDIT_TYPE_TRACE_COMMONS,
                &Some(CREDIT_SOURCE_TRACE_COMMONS.to_string()),
                &now,
                &Some(CHANGED_BY.to_string()),
                &Some(reason),
                &changed_by_user_id,
            ],
        )
        .await?;
        settle_unfunded_usage(&tx, organization_id, &self.allocation_policy).await?;
        tx.commit().await?;
        Ok(TraceCommonsCreditWrite {
            cumulative_nano_usd,
            applied: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_refs_are_bounded_and_printable() {
        assert!(validate_statement_ref("sha256:0123abcd").is_ok());
        assert!(validate_statement_ref("").is_err());
        assert!(validate_statement_ref("has space").is_err());
        assert!(validate_statement_ref(&"a".repeat(129)).is_err());
        assert!(validate_statement_ref("drop;table").is_err());
    }
}
