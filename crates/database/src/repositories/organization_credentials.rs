//! Organization-owned automation credentials. Permissions grant operations;
//! typed constraints only narrow the organization's administrative policy.
use crate::pool::DbPool;
use crate::repositories::credit_allocation::lock_organization_accounting;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_postgres::Transaction;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct OrganizationOperationError(pub &'static str);

pub const READ_CREDITS: &str = "credits:read";
pub const TRANSFER_CREDITS: &str = "credits:transfer";

#[derive(Clone)]
pub enum OrganizationActor {
    Owner(Uuid),
    Credential(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransferConstraints {
    pub max_amount_per_transfer: Option<i64>,
    pub max_total_amount: Option<i64>,
    pub allowed_source_types: Option<Vec<String>>,
    pub allowed_recipient_ids: Option<Vec<Uuid>>,
}

impl TransferConstraints {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_amount_per_transfer.is_some_and(|n| n < 0)
            || self.max_total_amount.is_some_and(|n| n < 0)
            || self.allowed_source_types.as_ref().is_some_and(|types| {
                types.len() > 2
                    || types
                        .iter()
                        .any(|t| !matches!(t.as_str(), "payment" | "staking_farm"))
            })
            || self
                .allowed_recipient_ids
                .as_ref()
                .is_some_and(|ids| ids.len() > 1000)
        {
            return Err(OrganizationOperationError("invalid_credential_constraints").into());
        }
        Ok(())
    }

    pub fn remaining(&self, spent: i64) -> i64 {
        self.max_total_amount
            .unwrap_or(i64::MAX)
            .saturating_sub(spent)
            .max(0)
    }

    pub fn allows_source(&self, source: &str) -> bool {
        self.allowed_source_types
            .as_ref()
            .is_none_or(|types| types.iter().any(|t| t == source))
    }

    pub fn allows(&self, amount: i64, source: &str, recipient: Uuid, spent: i64) -> bool {
        amount <= self.max_amount_per_transfer.unwrap_or(i64::MAX)
            && amount <= self.remaining(spent)
            && self.allows_source(source)
            && self
                .allowed_recipient_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(&recipient))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConstraints {
    #[serde(rename = "credits:transfer", default)]
    pub transfer: TransferConstraints,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRequest {
    pub name: String,
    pub permissions: Vec<String>,
    #[serde(default)]
    pub constraints: CredentialConstraints,
    pub expires_at: DateTime<Utc>,
}

impl CredentialRequest {
    fn validate(&self) -> anyhow::Result<()> {
        if self.name.trim().is_empty()
            || self.name.len() > 200
            || self.permissions.is_empty()
            || self.permissions.len() > 2
            || self
                .permissions
                .iter()
                .any(|p| !matches!(p.as_str(), READ_CREDITS | TRANSFER_CREDITS))
            || self.expires_at <= Utc::now()
            || self.expires_at > Utc::now() + chrono::Duration::days(366)
        {
            return Err(OrganizationOperationError("invalid_credential_configuration").into());
        }
        self.constraints.transfer.validate()
    }
}

pub struct Authorization {
    pub actor_id: Uuid,
    pub credential_id: Option<Uuid>,
    pub permissions: Vec<String>,
    pub constraints: TransferConstraints,
}

pub async fn authorize(
    tx: &Transaction<'_>,
    org: Uuid,
    actor: &OrganizationActor,
    permission: &str,
) -> anyhow::Result<Authorization> {
    authorize_inner(tx, org, actor, permission, true).await
}

/// Reject unrelated callers before acquiring accounting locks. Mutation callers
/// must recheck with authorize() after locking; readers use their MVCC snapshot.
pub async fn preauthorize(
    tx: &Transaction<'_>,
    org: Uuid,
    actor: &OrganizationActor,
    permission: &str,
) -> anyhow::Result<Authorization> {
    authorize_inner(tx, org, actor, permission, false).await
}

async fn authorize_inner(
    tx: &Transaction<'_>,
    org: Uuid,
    actor: &OrganizationActor,
    permission: &str,
    lock: bool,
) -> anyhow::Result<Authorization> {
    match actor {
        OrganizationActor::Owner(user) => {
            let mut query = String::from(
                r#"
                SELECT m.user_id
                FROM organization_members m
                JOIN organizations o ON o.id = m.organization_id
                WHERE m.organization_id = $1 AND m.user_id = $2
                  AND m.role = 'owner' AND o.is_active
                "#,
            );
            if lock {
                query.push_str(" FOR SHARE OF m");
            }
            let row = tx
                .query_opt(&query, &[&org, user])
                .await?
                .ok_or(OrganizationOperationError("credential_refused"))?;

            Ok(Authorization {
                actor_id: row.get(0),
                credential_id: None,
                permissions: vec![READ_CREDITS.into(), TRANSFER_CREDITS.into()],
                constraints: TransferConstraints::default(),
            })
        }
        OrganizationActor::Credential(hash) => {
            let mut query = String::from(
                r#"
                SELECT c.id, c.created_by, c.permissions, c.constraints
                FROM organization_credentials c
                JOIN organizations o ON o.id = c.organization_id
                WHERE c.organization_id = $1 AND c.token_hash = $2
                  AND c.revoked_at IS NULL AND c.expires_at > clock_timestamp() AND o.is_active
                "#,
            );
            if lock {
                query.push_str(" FOR SHARE OF c");
            }
            let row = tx
                .query_opt(&query, &[&org, hash])
                .await?
                .ok_or(OrganizationOperationError("credential_refused"))?;
            let permissions: Vec<String> = row.get("permissions");
            if !permissions.iter().any(|p| p == permission) {
                return Err(OrganizationOperationError("credential_refused").into());
            }
            let constraints: CredentialConstraints =
                serde_json::from_value(row.get("constraints"))?;

            Ok(Authorization {
                actor_id: row.get("created_by"),
                credential_id: Some(row.get("id")),
                permissions,
                constraints: constraints.transfer,
            })
        }
    }
}

pub fn hash_credential(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn secret() -> (String, String, String) {
    let token = format!("oc_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let prefix = token[..11].to_string();
    let hash = hash_credential(&token);
    (token, prefix, hash)
}

/// Ledger-derived lifetime usage survives configuration changes and rotation.
/// Caller holds the organization accounting lock, serializing all sends.
pub async fn transferred_amount(
    tx: &Transaction<'_>,
    org: Uuid,
    credential: Option<Uuid>,
) -> anyhow::Result<i64> {
    Ok(tx
        .query_one(
            r#"SELECT LEAST(COALESCE(SUM(amount::NUMERIC), 0), 9223372036854775807)::BIGINT
           FROM organization_credit_transfers
           WHERE sender_organization_id = $1 AND ($2::UUID IS NULL OR credential_id = $2)"#,
            &[&org, &credential],
        )
        .await?
        .get(0))
}

#[derive(Clone)]
pub struct OrganizationCredentialRepository {
    pool: DbPool,
}

impl OrganizationCredentialRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn list(&self, org: Uuid, owner: Uuid) -> anyhow::Result<Value> {
        let mut client = self.pool.get().await?;
        let tx = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        preauthorize(&tx, org, &OrganizationActor::Owner(owner), READ_CREDITS).await?;

        let rows = tx
            .query(
                r#"SELECT to_jsonb(c) - 'token_hash' AS credential,
                      COALESCE((SELECT SUM(t.amount::NUMERIC) FROM organization_credit_transfers t
                                WHERE t.credential_id = c.id), 0)::TEXT AS transferred_amount
               FROM organization_credentials c WHERE organization_id = $1
               ORDER BY created_at DESC, id DESC"#,
                &[&org],
            )
            .await?;
        let credentials = rows
            .iter()
            .map(|row| {
                let mut value: Value = row.get("credential");
                // Decimal strings preserve lifetime totals beyond JSON's safe integer range.
                value["transferred_amount"] = row.get::<_, String>("transferred_amount").into();
                value
            })
            .collect::<Vec<_>>();
        let audit = tx
            .query_one(
                r#"SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY created_at, id), '[]'::JSONB)
               FROM organization_credential_audit a WHERE organization_id = $1"#,
                &[&org],
            )
            .await?
            .get::<_, Value>(0);
        let management_enabled = management_enabled(&tx, org).await?;
        Ok(json!({
            "credentials": credentials,
            "audit": audit,
            "management_enabled": management_enabled,
        }))
    }

    pub async fn save(
        &self,
        org: Uuid,
        owner: Uuid,
        id: Option<Uuid>,
        request: &CredentialRequest,
    ) -> anyhow::Result<Value> {
        request.validate()?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        preauthorize(&tx, org, &OrganizationActor::Owner(owner), READ_CREDITS).await?;
        lock_organization_accounting(&tx, org).await?;
        authorize(&tx, org, &OrganizationActor::Owner(owner), READ_CREDITS).await?;
        if !management_enabled(&tx, org).await? {
            return Err(OrganizationOperationError("credential_management_disabled").into());
        }

        let constraints = serde_json::to_value(&request.constraints)?;
        let creating = id.is_none();
        let id = id.unwrap_or_else(Uuid::new_v4);
        let (token, prefix, hash) = secret();
        let row = if creating {
            tx.query_one(
                r#"INSERT INTO organization_credentials
                   (id, organization_id, name, permissions, constraints, token_hash, token_prefix, created_by, expires_at)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                   RETURNING to_jsonb(organization_credentials) - 'token_hash'"#,
                &[
                    &id, &org, &request.name, &request.permissions, &constraints,
                    &hash, &prefix, &owner, &request.expires_at,
                ],
            )
            .await?
        } else {
            tx.query_opt(
                r#"UPDATE organization_credentials SET name = $3, permissions = $4, constraints = $5, expires_at = $6
                   WHERE id = $1 AND organization_id = $2 AND revoked_at IS NULL
                   RETURNING to_jsonb(organization_credentials) - 'token_hash'"#,
                &[
                    &id, &org, &request.name, &request.permissions,
                    &constraints, &request.expires_at,
                ],
            )
            .await?
            .ok_or(OrganizationOperationError("credential_not_found"))?
        };
        let mut result: Value = row.get(0);
        audit(
            &tx,
            org,
            id,
            owner,
            if creating { "created" } else { "updated" },
            &result,
        )
        .await?;
        if creating {
            result["token"] = token.into();
        }
        tx.commit().await?;
        Ok(result)
    }

    pub async fn revoke(&self, org: Uuid, owner: Uuid, id: Uuid) -> anyhow::Result<Value> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        preauthorize(&tx, org, &OrganizationActor::Owner(owner), READ_CREDITS).await?;
        lock_organization_accounting(&tx, org).await?;
        authorize(&tx, org, &OrganizationActor::Owner(owner), READ_CREDITS).await?;
        let row = tx.query_opt(
            "SELECT revoked_at FROM organization_credentials WHERE organization_id = $1 AND id = $2",
            &[&org, &id],
        )
            .await?
            .ok_or(OrganizationOperationError("credential_not_found"))?;
        if row.get::<_, Option<DateTime<Utc>>>(0).is_none() {
            let result: Value = tx.query_one(
                r#"UPDATE organization_credentials SET revoked_at = clock_timestamp(), revoked_by = $3
                   WHERE organization_id = $1 AND id = $2
                   RETURNING to_jsonb(organization_credentials) - 'token_hash'"#,
                &[&org, &id, &owner],
            )
            .await?
            .get(0);
            audit(&tx, org, id, owner, "revoked", &result).await?;
        }
        tx.commit().await?;
        Ok(json!({"revoked": true}))
    }

    pub async fn rotate(&self, org: Uuid, owner: Uuid, id: Uuid) -> anyhow::Result<Value> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
            .await?;
        preauthorize(&tx, org, &OrganizationActor::Owner(owner), READ_CREDITS).await?;
        lock_organization_accounting(&tx, org).await?;
        authorize(&tx, org, &OrganizationActor::Owner(owner), READ_CREDITS).await?;
        if !management_enabled(&tx, org).await? {
            return Err(OrganizationOperationError("credential_management_disabled").into());
        }

        let (token, prefix, hash) = secret();
        let mut result: Value = tx.query_opt(
            r#"UPDATE organization_credentials SET token_hash = $3, token_prefix = $4
               WHERE organization_id = $1 AND id = $2 AND revoked_at IS NULL AND expires_at > clock_timestamp()
               RETURNING to_jsonb(organization_credentials) - 'token_hash'"#,
            &[&org, &id, &hash, &prefix],
        )
            .await?
            .ok_or(OrganizationOperationError("credential_not_found"))?.get(0);
        audit(&tx, org, id, owner, "rotated", &result).await?;
        result["token"] = token.into();
        tx.commit().await?;
        Ok(result)
    }
}

async fn audit(
    tx: &Transaction<'_>,
    org: Uuid,
    id: Uuid,
    owner: Uuid,
    action: &str,
    configuration: &Value,
) -> anyhow::Result<()> {
    tx.execute(
        r#"INSERT INTO organization_credential_audit
           (id, organization_id, credential_id, actor_id, action, configuration)
           VALUES ($1,$2,$3,$4,$5,$6)"#,
        &[&Uuid::new_v4(), &org, &id, &owner, &action, configuration],
    )
    .await?;
    Ok(())
}

async fn management_enabled(tx: &Transaction<'_>, org: Uuid) -> anyhow::Result<bool> {
    Ok(tx
        .query_one(
            r#"
        SELECT COALESCE((
            SELECT (policy #>> '{credentials,enabled}')::BOOLEAN
            FROM organization_policies
            WHERE organization_id = $1
        ), FALSE)
        "#,
            &[&org],
        )
        .await?
        .get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_configuration_rejects_unknown_authority_and_invalid_limits() {
        let valid = json!({
            "name": "Worker",
            "permissions": ["credits:transfer"],
            "constraints": {"credits:transfer": {"max_total_amount": 10}},
            "expires_at": Utc::now() + chrono::Duration::hours(1),
        });
        let request: CredentialRequest = serde_json::from_value(valid.clone()).unwrap();
        assert!(request.validate().is_ok());

        for permission in ["admin", "credentials:write", "credits:*", "reporting:read"] {
            let mut invalid = valid.clone();
            invalid["permissions"] = json!([permission]);
            let request: CredentialRequest = serde_json::from_value(invalid).unwrap();
            assert!(request.validate().is_err());
        }
        for limits in [
            json!({"max_total_amount": -1}),
            json!({"max_amount_per_transfer": -1}),
            json!({"allowed_source_types": ["transfer"]}),
        ] {
            let mut invalid = valid.clone();
            invalid["constraints"]["credits:transfer"] = limits;
            let request: CredentialRequest = serde_json::from_value(invalid).unwrap();
            assert!(request.validate().is_err());
        }
        let mut invalid = valid.clone();
        invalid["constraints"]["credits:transfer"]["max_total_amout"] = json!(10);
        assert!(serde_json::from_value::<CredentialRequest>(invalid).is_err());

        let mut expired: CredentialRequest = serde_json::from_value(valid).unwrap();
        expired.expires_at = Utc::now() - chrono::Duration::seconds(1);
        assert!(expired.validate().is_err());
    }
}
