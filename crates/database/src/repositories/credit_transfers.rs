//! Single-hop distribution of posted capacity. Every mutation locks both durable
//! accounting parents before reading balances; no network calls occur here.
use crate::pool::DbPool;
use crate::repositories::credit_allocation::{
    lock_organization_accounting, settle_unfunded_usage, CreditAllocationPolicy,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio_postgres::Transaction;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TransferError(pub &'static str);

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

#[derive(Clone)]
pub enum TransferActor {
    Owner(Uuid),
    Credential(String),
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

        let (actor_id, credential_id) = authorize(&tx, sender, actor).await?;
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
                SELECT policy_version
                FROM organization_credit_transfer_policies
                WHERE organization_id = $1
                AND enabled
                "#,
                &[&sender],
            )
            .await?
            .ok_or(TransferError("sender_not_enabled"))?;
        let policy_version: i64 = policy.get(0);
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
        if recent >= 60 {
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
        let tx = client.transaction().await?;
        lock_organization_accounting(&tx, org).await?;
        authorize(&tx, org, actor).await?;
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
        Ok(
            json!({"credits": credits, "own_staking_earned_total": own_earned, "sender_reconciled": reconciled}),
        )
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
        let tx = client.transaction().await?;
        lock_organization_accounting(&tx, org).await?;
        authorize(&tx, org, actor).await?;
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

    pub async fn policy(
        &self,
        org: Uuid,
        update: Option<(bool, Uuid, &str)>,
    ) -> anyhow::Result<Value> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        lock_organization_accounting(&tx, org).await?;
        if let Some((enabled, admin, reason)) = update {
            if reason.trim().is_empty() || reason.len() > 2000 {
                return reject("invalid_policy_reason");
            }
            tx
            .execute(
                r#"WITH policy AS (
                    INSERT INTO organization_credit_transfer_policies
                        (organization_id, enabled, policy_version, changed_by, change_reason)
                    VALUES ($1, $2, 1, $3, $4)
                    ON CONFLICT (organization_id) DO UPDATE SET
                        enabled = EXCLUDED.enabled,
                        policy_version = organization_credit_transfer_policies.policy_version + 1,
                        changed_by = EXCLUDED.changed_by, change_reason = EXCLUDED.change_reason,
                        changed_at = clock_timestamp()
                    RETURNING *
                ) INSERT INTO organization_credit_transfer_policy_audit
                    (id, organization_id, enabled, policy_version, changed_by, change_reason)
                  SELECT $5, organization_id, enabled, policy_version, changed_by, change_reason FROM policy"#,
                &[&org, &enabled, &admin, &reason, &Uuid::new_v4()],
            )
            .await?;
        }
        let result: Value = tx
            .query_one(
                r#"SELECT jsonb_build_object(
                'policy', COALESCE((
                    SELECT to_jsonb(p)
                    FROM organization_credit_transfer_policies p
                    WHERE organization_id = $1
                ), '{"enabled":false}'::JSONB),
                'audit', COALESCE((
                    SELECT jsonb_agg(to_jsonb(a) ORDER BY policy_version)
                    FROM organization_credit_transfer_policy_audit a
                    WHERE organization_id = $1
                ), '[]'::JSONB))"#,
                &[&org],
            )
            .await?
            .get(0);
        tx.commit().await?;
        Ok(result)
    }

    pub async fn credential(
        &self,
        org: Uuid,
        owner: Uuid,
        revoke: Option<Uuid>,
        expires_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Value> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        lock_organization_accounting(&tx, org).await?;
        authorize(&tx, org, &TransferActor::Owner(owner)).await?;
        let result = if let Some(id) = revoke {
            tx.execute(
                r#"
                UPDATE organization_credit_transfer_credentials
                SET revoked_at = clock_timestamp()
                WHERE id = $1
                AND organization_id = $2
                "#,
                &[&id, &org],
            )
            .await?;
            json!({"revoked": true})
        } else {
            let expires_at = expires_at.ok_or(TransferError("invalid_credential_expiry"))?;
            if expires_at <= Utc::now() || expires_at > Utc::now() + chrono::Duration::days(366) {
                return reject("invalid_credential_expiry");
            }
            let id = Uuid::new_v4();
            let token = format!("ct_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let hash = hash_credential(&token);
            tx
            .execute(
                r#"
                INSERT INTO organization_credit_transfer_credentials (id, organization_id, token_hash, created_by, expires_at)
                VALUES ($1,$2,$3,$4,$5)
                "#,
                &[&id, &org, &hash, &owner, &expires_at],
            )
            .await?;
            json!({"id": id, "token": token, "expires_at": expires_at, "scope": "credits:transfer"})
        };
        tx.commit().await?;
        Ok(result)
    }
}

pub fn hash_credential(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

async fn authorize(
    tx: &Transaction<'_>,
    org: Uuid,
    actor: &TransferActor,
) -> anyhow::Result<(Uuid, Option<Uuid>)> {
    // Row-level SHARE locks define the authorization boundary: concurrent
    // membership changes/deletion and credential revocation wait for this tx.
    // Token authentication by middleware alone never authorizes a money move.
    match actor {
        TransferActor::Owner(user) => {
            let row = tx
            .query_opt(
                r#"SELECT m.user_id FROM organization_members m
                   JOIN organizations o ON o.id = m.organization_id
                   WHERE m.organization_id = $1 AND m.user_id = $2 AND m.role = 'owner' AND o.is_active
                   FOR SHARE OF m"#,
                &[&org, user],
            )
            .await?;
            row.map(|_| (*user, None))
                .ok_or_else(|| TransferError("credential_refused").into())
        }
        TransferActor::Credential(hash) => {
            let row = tx
                .query_opt(
                    r#"SELECT c.id, c.created_by FROM organization_credit_transfer_credentials c
                   JOIN organizations o ON o.id = c.organization_id
                   WHERE c.organization_id = $1 AND c.token_hash = $2
                     AND c.revoked_at IS NULL AND c.expires_at > clock_timestamp() AND o.is_active
                   FOR SHARE OF c"#,
                    &[&org, hash],
                )
                .await?
                .ok_or(TransferError("credential_refused"))?;
            Ok((row.get("created_by"), Some(row.get("id"))))
        }
    }
}

async fn snapshot(tx: &Transaction<'_>, org: Uuid) -> anyhow::Result<Value> {
    Ok(tx
        .query_one(
            r#"SELECT COALESCE(jsonb_agg(jsonb_build_object(
            'credit_type', c.credit_type, 'gross_limit', c.spend_limit,
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

        for source in ["grant", "postpay", "transfer", "trace_commons"] {
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
