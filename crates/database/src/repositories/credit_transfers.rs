//! Single-hop distribution of posted capacity. Every mutation locks both durable
//! accounting parents before reading balances; no network calls occur here.
use crate::pool::DbPool;
use crate::repositories::credit_allocation::{
    lock_organization_accounting, settle_unfunded_usage, CreditAllocationPolicy,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::organization_credentials::{
    authorize, preauthorize, transferred_amount, TransferConstraints, READ_CREDITS,
    TRANSFER_CREDITS,
};
pub use super::organization_credentials::{hash_credential, OrganizationActor as TransferActor};
use tokio_postgres::Transaction;
use uuid::Uuid;

pub use super::organization_credentials::OrganizationOperationError as TransferError;

fn reject<T>(code: &'static str) -> anyhow::Result<T> {
    Err(TransferError(code).into())
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    pub recipient_organization_id: Uuid,
    pub source_credit_type: String,
    pub amount: i64,
    #[serde(default)]
    pub memo: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

impl TransferRequest {
    pub fn validate(&self, sender: Uuid, key: &str) -> anyhow::Result<Value> {
        if sender == self.recipient_organization_id
            || !matches!(self.source_credit_type.as_str(), "payment" | "staking_farm")
            || self.amount <= 0
            || key.trim().is_empty()
            || key.trim().chars().count() > 100
            || self.memo.len() > 2000
        {
            return reject("invalid_transfer_terms");
        }

        // Recursively sort object keys. Arrays retain their order. Persist this
        // exact normalized representation and compare JSON values on replay.
        fn canonical(value: Value, depth: usize) -> anyhow::Result<Value> {
            if depth > 16 {
                return reject("invalid_metadata_depth");
            }
            Ok(match value {
                Value::Object(object) => {
                    let sorted = object
                        .into_iter()
                        .collect::<std::collections::BTreeMap<_, _>>();
                    Value::Object(
                        sorted
                            .into_iter()
                            .map(|(key, value)| Ok((key, canonical(value, depth + 1)?)))
                            .collect::<anyhow::Result<_>>()?,
                    )
                }
                Value::Array(array) => Value::Array(
                    array
                        .into_iter()
                        .map(|value| canonical(value, depth + 1))
                        .collect::<anyhow::Result<_>>()?,
                ),
                scalar => scalar,
            })
        }

        let metadata = canonical(Value::Object(self.metadata.clone()), 0)?;
        if serde_json::to_vec(&metadata)?.len() > 16 * 1024 {
            return reject("invalid_metadata_size");
        }

        Ok(json!({
            "recipient_organization_id": self.recipient_organization_id,
            "source_credit_type": self.source_credit_type,
            "amount": self.amount,
            "memo": self.memo,
            "metadata": metadata,
        }))
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPolicy {
    #[serde(default)]
    pub credentials: CredentialPolicy,
    #[serde(default)]
    pub credits_transfer: TransferPolicy,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialPolicy {
    pub enabled: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransferPolicy {
    pub enabled: bool,
    #[serde(default)]
    pub constraints: TransferConstraints,
    #[serde(default = "default_send_quota")]
    pub max_sends_per_minute: i64,
}

fn default_send_quota() -> i64 {
    60
}

impl Default for TransferPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            constraints: TransferConstraints::default(),
            max_sends_per_minute: 60,
        }
    }
}

pub struct TransferHistoryOptions {
    pub limit: i64,
    pub direction: Option<String>,
}

impl From<i64> for TransferHistoryOptions {
    fn from(limit: i64) -> Self {
        Self {
            limit,
            direction: None,
        }
    }
}

#[derive(Clone)]
pub struct CreditTransferRepository {
    pool: DbPool,
    policy: CreditAllocationPolicy,
}

impl CreditTransferRepository {
    pub fn new(pool: DbPool, config: &config::CreditAllocationConfig) -> Self {
        Self {
            pool,
            policy: CreditAllocationPolicy::from(config),
        }
    }

    pub async fn transfer(
        &self,
        sender: Uuid,
        actor: &TransferActor,
        key: &str,
        request: &TransferRequest,
    ) -> anyhow::Result<(Value, bool)> {
        // Bound the whole operation, not only individual SQL statements. If a
        // commit response is lost at the deadline, the stable key resolves it.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.transfer_transaction(sender, actor, key, request),
        )
        .await
        .map_err(|_| TransferError("accounting_busy"))?
    }

    async fn transfer_transaction(
        &self,
        sender: Uuid,
        actor: &TransferActor,
        key: &str,
        request: &TransferRequest,
    ) -> anyhow::Result<(Value, bool)> {
        let key = key.trim();
        let terms = request.validate(sender, key)?;
        let recipient = request.recipient_organization_id;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;

        preauthorize(&tx, sender, actor, TRANSFER_CREDITS).await?;

        let mut recipient_active = false;
        for org in [sender.min(recipient), sender.max(recipient)] {
            let active = tx
                .query_opt(
                    "SELECT is_active FROM organizations WHERE id = $1 FOR UPDATE",
                    &[&org],
                )
                .await?;
            if org == recipient {
                recipient_active = active.is_some_and(|row| row.get::<_, bool>(0));
            }
        }

        let authorization = authorize(&tx, sender, actor, TRANSFER_CREDITS).await?;
        let actor_id = authorization.actor_id;
        let credential_id = authorization.credential_id;
        if let Some(row) = tx
            .query_opt(
                r#"
                SELECT terms = $3::JSONB AS matches, result
                FROM organization_credit_transfers
                WHERE sender_organization_id = $1
                AND idempotency_key = $2
                "#,
                &[&sender, &key, &terms],
            )
            .await?
        {
            if !row.get::<_, bool>("matches") {
                return reject("idempotency_conflict");
            }
            return Ok((row.get("result"), true));
        }

        let policy = tx
            .query_opt(
                r#"
                SELECT policy_version, policy
                FROM organization_policies
                WHERE organization_id = $1
                "#,
                &[&sender],
            )
            .await?
            .ok_or(TransferError("sender_not_enabled"))?;
        let policy_version: i64 = policy.get(0);
        let organization_policy: OrganizationPolicy = serde_json::from_value(policy.get(1))?;
        let transfer_policy = organization_policy.credits_transfer;
        if !transfer_policy.enabled {
            return reject("sender_not_enabled");
        }
        let organization_spent = transferred_amount(&tx, sender, None).await?;
        if !transfer_policy.constraints.allows(
            request.amount,
            &request.source_credit_type,
            recipient,
            organization_spent,
        ) {
            return reject("organization_transfer_limit_exceeded");
        }
        if let Some(id) = credential_id {
            let spent = transferred_amount(&tx, sender, Some(id)).await?;
            if !authorization.constraints.allows(
                request.amount,
                &request.source_credit_type,
                recipient,
                spent,
            ) {
                return reject("credential_transfer_limit_exceeded");
            }
        }
        if !recipient_active {
            return reject("organization_unavailable");
        }

        let balance = tx
            .query_opt(
                r#"
                SELECT legacy_unattributed_amount, unresolved_unfunded_amount
                FROM organization_balance
                WHERE organization_id = $1
                "#,
                &[&sender],
            )
            .await?
            .ok_or(TransferError("sender_not_reconciled"))?;
        if balance.get::<_, i64>(0) != 0 || balance.get::<_, i64>(1) != 0 {
            return reject("sender_not_reconciled");
        }

        // A durable per-sender rate limit serializes with sends and is unaffected
        // by application replica count. Exact retries above do not consume it.
        let recent: i64 = tx
            .query_one(
                r#"
                SELECT count(*)
                FROM organization_credit_transfers
                WHERE sender_organization_id = $1
                AND created_at > clock_timestamp() - INTERVAL '1 minute'
                "#,
                &[&sender],
            )
            .await?
            .get(0);
        if recent >= transfer_policy.max_sends_per_minute {
            return reject("transfer_rate_limited");
        }

        let source = tx
            .query_opt(
                r#"SELECT c.id, c.effective_capacity, COALESCE(u.amount, 0)::BIGINT AS consumed
               FROM organization_credit_capacities c
               LEFT JOIN organization_credit_consumption u
                 ON u.organization_id = c.organization_id AND u.credit_type = c.credit_type
               WHERE c.organization_id = $1 AND c.credit_type = $2
                 AND c.effective_until IS NULL AND c.currency = 'USD'
                 AND (c.credit_type <> 'staking_farm' OR c.source = 'house-of-stake')"#,
                &[&sender, &request.source_credit_type],
            )
            .await?
            .ok_or(TransferError("insufficient_posted_capacity"))?;
        if source
            .get::<_, i64>("effective_capacity")
            .saturating_sub(source.get::<_, i64>("consumed"))
            < request.amount
        {
            return reject("insufficient_posted_capacity");
        }
        let source_limit_id: Uuid = source.get("id");
        let staking_evidence = if request.source_credit_type == "staking_farm" {
            Some(
                tx
            .query_one(
                    r#"SELECT jsonb_build_object(
                    'own_earned_total', (SELECT COALESCE(MAX(spend_limit), 0) FROM organization_limits_history
                        WHERE organization_id = $1 AND credit_type = 'staking_farm' AND source = 'house-of-stake'),
                    'sync_observed_at', MAX(last_synced_at),
                    'chain_accrual_cutoff', NULL)
                   FROM organization_staking_farm_sources WHERE organization_id = $1"#,
                    &[&sender],
                )
                .await?
                .get::<_, Value>(0),
            )
        } else {
            None
        };

        let incoming_before: i64 = tx
            .query_one(
                r#"
                SELECT COALESCE((SELECT incoming
                FROM organization_credit_transfer_totals
                WHERE organization_id = $1
                AND credit_type = 'transfer'), 0)::BIGINT
                "#,
                &[&recipient],
            )
            .await?
            .get(0);
        if incoming_before.checked_add(request.amount).is_none() {
            return reject("capacity_overflow");
        }

        tx
            .execute(
            r#"INSERT INTO organization_credit_transfer_totals (organization_id, credit_type, outgoing)
               VALUES ($1, $2, $3)
               ON CONFLICT (organization_id, credit_type) DO UPDATE
               SET outgoing = organization_credit_transfer_totals.outgoing + EXCLUDED.outgoing"#,
            &[&sender, &request.source_credit_type, &request.amount],
        )
            .await?;
        let incoming: i64 = tx
            .query_one(
            r#"INSERT INTO organization_credit_transfer_totals (organization_id, credit_type, incoming)
               VALUES ($1, 'transfer', $2)
               ON CONFLICT (organization_id, credit_type) DO UPDATE
               SET incoming = organization_credit_transfer_totals.incoming + EXCLUDED.incoming
               RETURNING incoming"#,
            &[&recipient, &request.amount],
        )
            .await?
            .get(0);

        tx.execute(
            r#"
                UPDATE organization_limits_history
                SET effective_until = clock_timestamp()
                WHERE organization_id = $1
                AND credit_type = 'transfer'
                AND effective_until IS NULL
                "#,
            &[&recipient],
        )
        .await?;
        let recipient_limit_id: Uuid = tx
            .query_one(
            r#"INSERT INTO organization_limits_history
                (organization_id, spend_limit, credit_type, source, currency, changed_by, change_reason)
               VALUES ($1, $2, 'transfer', 'credit_transfer', 'USD', 'system', 'Completed incoming transfer')
               RETURNING id"#,
            &[&recipient, &incoming],
        )
            .await?
            .get(0);

        let settled = settle_unfunded_usage(&tx, recipient, &self.policy).await?;
        let sender_snapshot = snapshot(&tx, sender).await?;
        let recipient_snapshot = json!({
            "credits": snapshot(&tx, recipient).await?,
            "amount_applied_to_debt": settled,
        });
        let id = Uuid::new_v4();
        let created_at: DateTime<Utc> = tx.query_one("SELECT clock_timestamp()", &[]).await?.get(0);
        // The sender sees delivery and its own position only. Recipient debt and
        // unrelated balances stay in the separately authorized stored snapshot.
        let result = json!({
            "id": id,
            "sender_organization_id": sender,
            "recipient_organization_id": recipient,
            "source_credit_type": request.source_credit_type,
            "destination_credit_type": "transfer",
            "amount": request.amount,
            "memo": request.memo,
            "metadata": terms["metadata"],
            "created_at": created_at,
            "staking_evidence": staking_evidence,
            "source_limit_id": source_limit_id,
            "sender_snapshot": sender_snapshot,
            "delivered": true,
            "schema_version": 1,
        });
        let actor_kind = if credential_id.is_some() {
            "credits:transfer"
        } else {
            "owner_session"
        };
        tx.execute(
            r#"INSERT INTO organization_credit_transfers (
                id, sender_organization_id, recipient_organization_id, source_credit_type,
                amount, idempotency_key, terms, actor_id, actor_kind, credential_id,
                policy_version, source_limit_id, recipient_limit_id, staking_evidence,
                result, recipient_snapshot, created_at
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)"#,
            &[
                &id,
                &sender,
                &recipient,
                &request.source_credit_type,
                &request.amount,
                &key,
                &terms,
                &actor_id,
                &actor_kind,
                &credential_id,
                &policy_version,
                &source_limit_id,
                &recipient_limit_id,
                &staking_evidence,
                &result,
                &recipient_snapshot,
                &created_at,
            ],
        )
        .await?;

        tx.commit().await?;
        Ok((result, false))
    }

    pub async fn capacity(&self, org: Uuid, actor: &TransferActor) -> anyhow::Result<Value> {
        let mut client = self.pool.get().await?;
        let tx = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'")
            .await?;
        let authorization = preauthorize(&tx, org, actor, READ_CREDITS).await?;
        let credits = snapshot(&tx, org).await?;
        let own_earned: i64 = tx
            .query_one(
                r#"
                SELECT COALESCE(MAX(spend_limit), 0)::BIGINT
                FROM organization_limits_history
                WHERE organization_id = $1
                AND credit_type = 'staking_farm'
                AND source = 'house-of-stake'
                "#,
                &[&org],
            )
            .await?
            .get(0);
        let reconciled: bool = tx
            .query_one(
                r#"
                SELECT EXISTS (SELECT 1
                FROM organization_balance
                WHERE organization_id = $1
                AND legacy_unattributed_amount = 0
                AND unresolved_unfunded_amount = 0)
                "#,
                &[&org],
            )
            .await?
            .get(0);
        let policy = load_policy(&tx, org).await?;
        let organization_spent = transferred_amount(&tx, org, None).await?;
        let organization_remaining = policy
            .credits_transfer
            .constraints
            .remaining(organization_spent);
        let credential_spent = match authorization.credential_id {
            Some(id) => transferred_amount(&tx, org, Some(id)).await?,
            None => 0,
        };
        let credential_remaining = authorization.constraints.remaining(credential_spent);
        let can_send = policy.credits_transfer.enabled
            && reconciled
            && authorization
                .permissions
                .iter()
                .any(|p| p == TRANSFER_CREDITS);
        let recent: i64 = tx
            .query_one(
                r#"
                SELECT COUNT(*)
                FROM organization_credit_transfers
                WHERE sender_organization_id = $1
                  AND created_at > clock_timestamp() - INTERVAL '1 minute'
                "#,
                &[&org],
            )
            .await?
            .get(0);
        let sends_remaining = policy
            .credits_transfer
            .max_sends_per_minute
            .saturating_sub(recent)
            .max(0);
        let mut sources = Vec::new();
        for source in ["payment", "staking_farm"] {
            let available = credits
                .as_array()
                .unwrap()
                .iter()
                .find(|credit| {
                    credit["credit_type"] == source
                        && (source != "staking_farm" || credit["source"] == "house-of-stake")
                })
                .and_then(|credit| credit["available"].as_i64())
                .unwrap_or(0);
            let allowed = policy.credits_transfer.constraints.allows_source(source)
                && authorization.constraints.allows_source(source);
            let maximum = if can_send && allowed && sends_remaining > 0 {
                available
                    .min(organization_remaining)
                    .min(credential_remaining)
                    .min(
                        policy
                            .credits_transfer
                            .constraints
                            .max_amount_per_transfer
                            .unwrap_or(i64::MAX),
                    )
                    .min(
                        authorization
                            .constraints
                            .max_amount_per_transfer
                            .unwrap_or(i64::MAX),
                    )
            } else {
                0
            };
            sources.push(json!({"source_credit_type": source, "max_amount_per_transfer": maximum}));
        }
        Ok(json!({
            "credits": credits,
            "own_staking_earned_total": own_earned,
            "sender_reconciled": reconciled,
            "sending_enabled": policy.credits_transfer.enabled,
            "credential_can_transfer": authorization.permissions.iter().any(|p| p == TRANSFER_CREDITS),
            "organization_constraints": policy.credits_transfer.constraints,
            "credential_constraints": authorization.constraints,
            "organization_remaining_budget": organization_remaining,
            "credential_remaining_budget": credential_remaining,
            "sends_remaining_in_window": sends_remaining,
            "sources": sources,
            "recipient_restrictions_apply": true,
            "is_reservation": false,
        }))
    }

    pub async fn history(
        &self,
        org: Uuid,
        actor: &TransferActor,
        id: Option<Uuid>,
        key: Option<&str>,
        before: Option<(DateTime<Utc>, Uuid)>,
        options: impl Into<TransferHistoryOptions>,
    ) -> anyhow::Result<Value> {
        let options = options.into();
        if !matches!(
            options.direction.as_deref(),
            None | Some("incoming" | "outgoing")
        ) {
            return reject("invalid_direction");
        }
        let mut client = self.pool.get().await?;
        let tx = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'")
            .await?;
        preauthorize(&tx, org, actor, READ_CREDITS).await?;
        let (before_time, before_id) =
            before.map_or((None, None), |(time, id)| (Some(time), Some(id)));
        let rows = tx
            .query(
            r#"SELECT sender_organization_id, result, recipient_snapshot, id, created_at, idempotency_key
               FROM organization_credit_transfers
               WHERE (sender_organization_id = $1 OR recipient_organization_id = $1)
                 AND ($2::UUID IS NULL OR id = $2)
                 AND ($3::TEXT IS NULL OR (sender_organization_id = $1 AND idempotency_key = $3))
                 AND ($4::TIMESTAMPTZ IS NULL OR (created_at, id) < ($4, $5::UUID))
                 AND ($7::TEXT IS NULL
                      OR ($7 = 'outgoing' AND sender_organization_id = $1)
                      OR ($7 = 'incoming' AND recipient_organization_id = $1))
               ORDER BY created_at DESC, id DESC LIMIT $6"#,
            &[&org, &id, &key, &before_time, &before_id, &options.limit.clamp(1, 100), &options.direction],
        )
            .await?;
        let items = rows
            .iter()
            .map(|row| {
                let mut result: Value = row.get("result");
                if row.get::<_, Uuid>("sender_organization_id") == org {
                    result["idempotency_key"] = row.get::<_, String>("idempotency_key").into();
                } else {
                    result.as_object_mut().unwrap().remove("sender_snapshot");
                    result.as_object_mut().unwrap().remove("staking_evidence");
                    result["recipient_snapshot"] = row.get("recipient_snapshot");
                }
                result
            })
            .collect::<Vec<_>>();
        let next = rows.last().map(|row| {
            json!({
                "before_time": row.get::<_, DateTime<Utc>>("created_at"),
                "before_id": row.get::<_, Uuid>("id"),
            })
        });
        Ok(json!({"transfers": items, "next_cursor": next}))
    }

    /// Convenience for internal callers changing only enablement.
    pub async fn policy(
        &self,
        org: Uuid,
        update: Option<(bool, Uuid, &str)>,
    ) -> anyhow::Result<Value> {
        self.configure_policy(
            org,
            update.map(|(enabled, actor, reason)| {
                (
                    OrganizationPolicy {
                        credentials: CredentialPolicy { enabled: true },
                        credits_transfer: TransferPolicy {
                            enabled,
                            ..Default::default()
                        },
                    },
                    actor,
                    reason,
                )
            }),
        )
        .await
    }

    pub async fn configure_policy(
        &self,
        org: Uuid,
        update: Option<(OrganizationPolicy, Uuid, &str)>,
    ) -> anyhow::Result<Value> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        lock_organization_accounting(&tx, org).await?;
        if let Some((policy, admin, reason)) = update {
            if reason.trim().is_empty()
                || reason.len() > 2000
                || !(1..=100_000).contains(&policy.credits_transfer.max_sends_per_minute)
            {
                return reject("invalid_policy_configuration");
            }
            policy.credits_transfer.constraints.validate()?;
            let policy = serde_json::to_value(policy)?;
            tx.execute(
                r#"WITH saved AS (
                    INSERT INTO organization_policies
                        (organization_id, policy, policy_version, changed_by, change_reason)
                    VALUES ($1, $2, 1, $3, $4)
                    ON CONFLICT (organization_id) DO UPDATE SET
                        policy = EXCLUDED.policy,
                        policy_version = organization_policies.policy_version + 1,
                        changed_by = EXCLUDED.changed_by, change_reason = EXCLUDED.change_reason,
                        changed_at = clock_timestamp()
                    RETURNING *
                ) INSERT INTO organization_policy_audit
                    (id, organization_id, policy, policy_version, changed_by, change_reason)
                  SELECT $5, organization_id, policy, policy_version, changed_by, change_reason FROM saved"#,
                &[&org, &policy, &admin, &reason, &Uuid::new_v4()],
            ).await?;
        }
        let policy = load_policy(&tx, org).await?;
        let audit: Value = tx
            .query_one(
                r#"SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY policy_version), '[]'::JSONB)
               FROM organization_policy_audit a WHERE organization_id = $1"#,
                &[&org],
            )
            .await?
            .get(0);
        tx.commit().await?;
        Ok(json!({"policy": policy, "audit": audit}))
    }

    /// A single statement gives the operator a consistent snapshot without SQL access.
    pub async fn reconciliation(&self, org: Uuid) -> anyhow::Result<Value> {
        let client = self.pool.get().await?;
        Ok(client
            .query_one(
                include_str!("sql/credit_transfer_reconciliation.sql"),
                &[&org],
            )
            .await?
            .get(0))
    }
}

async fn load_policy(tx: &Transaction<'_>, org: Uuid) -> anyhow::Result<OrganizationPolicy> {
    match tx
        .query_opt(
            "SELECT policy FROM organization_policies WHERE organization_id = $1",
            &[&org],
        )
        .await?
    {
        Some(row) => Ok(serde_json::from_value(row.get(0))?),
        None => Ok(OrganizationPolicy::default()),
    }
}

async fn snapshot(tx: &Transaction<'_>, org: Uuid) -> anyhow::Result<Value> {
    Ok(tx
        .query_one(
            r#"SELECT COALESCE(jsonb_agg(jsonb_build_object(
            'credit_type', c.credit_type, 'source', c.source, 'gross_limit', c.spend_limit,
            'transferred_out', c.transferred_out, 'effective_capacity', c.effective_capacity,
            'consumed', COALESCE(u.amount, 0),
            'available', GREATEST(c.effective_capacity - COALESCE(u.amount, 0), 0)
        ) ORDER BY c.credit_type), '[]'::JSONB)
        FROM organization_credit_capacities c
        LEFT JOIN organization_credit_consumption u
          ON u.organization_id = c.organization_id AND u.credit_type = c.credit_type
        WHERE c.organization_id = $1 AND c.effective_until IS NULL"#,
            &[&org],
        )
        .await?
        .get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_terms_preserve_text_and_reject_invalid_fields() {
        let sender = Uuid::new_v4();
        let recipient = Uuid::new_v4();
        let payload = json!({
            "recipient_organization_id": recipient,
            "source_credit_type": "payment",
            "amount": 1,
        });
        let request: TransferRequest = serde_json::from_value(payload.clone()).unwrap();

        let normalized = request.validate(sender, " key ").unwrap();

        assert_eq!(normalized["memo"], "");
        assert_eq!(normalized["metadata"], json!({}));
        assert!(request.validate(sender, " ").is_err());
        assert!(request.validate(recipient, "key").is_err());
        assert!(request.validate(sender, &"k".repeat(101)).is_err());

        for (field, invalid) in [
            ("memo", Value::Null),
            ("memo", json!(12)),
            ("metadata", Value::Null),
            ("metadata", json!([])),
            ("metadata", json!("{}")),
        ] {
            let mut invalid_payload = payload.clone();
            invalid_payload[field] = invalid;

            assert!(serde_json::from_value::<TransferRequest>(invalid_payload).is_err());
        }

        for source in ["grant", "postpay", "transfer"] {
            let mut invalid = request.clone();
            invalid.source_credit_type = source.into();

            assert!(invalid.validate(sender, "key").is_err());
        }

        let mut request = request;
        request.memo = "  unchanged\n".into();
        request.metadata = serde_json::from_str(r#"{"z": [2, 1], "a": {"y": 2, "x": 1}}"#).unwrap();
        let first = request.validate(sender, "key").unwrap();
        request.metadata = serde_json::from_str(r#"{"a": {"x": 1, "y": 2}, "z": [2, 1]}"#).unwrap();

        assert_eq!(first, request.validate(sender, "key").unwrap());
        assert_eq!(first["memo"], "  unchanged\n");

        request
            .metadata
            .insert("large".into(), json!("x".repeat(16 * 1024)));
        assert!(request.validate(sender, "key").is_err());

        request.metadata.clear();
        let mut nested = json!(0);
        for _ in 0..17 {
            nested = json!({"nested": nested});
        }
        request.metadata.insert("nested".into(), nested);
        assert!(request.validate(sender, "key").is_err());
    }
}
