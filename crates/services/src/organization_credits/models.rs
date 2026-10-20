//! Domain types and pure validation shared by services and transactional adapters.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub use OrganizationActor as TransferActor;
pub use OrganizationOperationError as TransferError;

fn reject<T>(code: &'static str) -> anyhow::Result<T> {
    Err(TransferError(code).into())
}

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
                types.len() > 3
                    || types
                        .iter()
                        .any(|t| !matches!(t.as_str(), "payment" | "staking_farm" | "grant"))
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
    pub fn validate(&self) -> anyhow::Result<()> {
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

pub fn hash_credential(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    pub recipient_organization_id: Uuid,
    pub source_credit_type: String,
    pub amount: Option<i64>,
    pub recipient_cumulative: Option<i64>,
    pub payout_program: Option<String>,
    #[serde(default)]
    pub memo: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

impl TransferRequest {
    pub fn validate(&self, sender: Uuid, key: &str) -> anyhow::Result<Value> {
        if sender == self.recipient_organization_id
            || !matches!(
                self.source_credit_type.as_str(),
                "payment" | "staking_farm" | "grant"
            )
            || !matches!(
                (self.amount, self.recipient_cumulative),
                (Some(1..), None) | (None, Some(0..))
            )
            || (self.amount.is_some() && key.trim().is_empty())
            || (self.recipient_cumulative.is_some()
                && (self.payout_program.is_none() || !key.is_empty()))
            || self.payout_program.as_ref().is_some_and(|program| {
                program.is_empty() || program.len() > 100 || program.trim() != program
            })
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
            "recipient_cumulative": self.recipient_cumulative,
            "payout_program": self.payout_program,
            "memo": self.memo,
            "metadata": metadata,
        }))
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPolicy {
    pub credentials: CredentialPolicy,
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

impl TransferPolicy {
    pub fn allows_source(&self, source: &str) -> bool {
        match &self.constraints.allowed_source_types {
            Some(types) => types.iter().any(|kind| kind == source),
            None => matches!(source, "payment" | "staking_farm"),
        }
    }
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

// This early rejection is advisory; repeat it after accounting locks. Budget
// usage and cumulative deltas are checked only against the locked position.
pub fn check_send_policy(
    policy: &TransferPolicy,
    authorization: &Authorization,
    request: &TransferRequest,
) -> anyhow::Result<()> {
    if !policy.enabled {
        return reject("sender_not_enabled");
    }

    let amount = request.amount.unwrap_or(0);
    if !policy.allows_source(&request.source_credit_type)
        || !policy.constraints.allows(
            amount,
            &request.source_credit_type,
            request.recipient_organization_id,
            0,
        )
    {
        return reject("organization_transfer_limit_exceeded");
    }

    if authorization.credential_id.is_some()
        && !authorization.constraints.allows(
            amount,
            &request.source_credit_type,
            request.recipient_organization_id,
            0,
        )
    {
        return reject("credential_transfer_limit_exceeded");
    }

    Ok(())
}

impl OrganizationPolicy {
    pub fn validate(&self, reason: &str) -> anyhow::Result<()> {
        if reason.trim().is_empty()
            || reason.len() > 2000
            || !(1..=100_000).contains(&self.credits_transfer.max_sends_per_minute)
        {
            return reject("invalid_policy_configuration");
        }

        self.credits_transfer.constraints.validate()
    }
}

/// Filters for one authorized transfer history snapshot.
pub struct HistoryRequest {
    pub id: Option<Uuid>,
    pub key: Option<String>,
    pub before: Option<(DateTime<Utc>, Uuid)>,
    pub options: TransferHistoryOptions,
}

impl HistoryRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !matches!(
            self.options.direction.as_deref(),
            None | Some("incoming" | "outgoing")
        ) {
            return reject("invalid_direction");
        }
        Ok(())
    }
}
