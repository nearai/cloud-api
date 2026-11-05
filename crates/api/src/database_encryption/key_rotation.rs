use super::{bad, decrypt_ring_envelope, envelope, Field, KeyRing, Mode, MARKER};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Classification {
    Plaintext,
    Current,
    Stale(String),
    Invalid,
}

impl Classification {
    pub(super) fn key_id<'a>(&self, ring: &'a KeyRing) -> Option<&'a str> {
        match self {
            Self::Current => Some(ring.write_id()),
            Self::Stale(key_id) => ring.canonical_id(key_id),
            Self::Invalid | Self::Plaintext => None,
        }
    }
}

pub(super) fn classify(ring: &KeyRing, field: &Field, id: Uuid, value: &Value) -> Classification {
    if value[MARKER] != true {
        return Classification::Plaintext;
    }
    if database::field_encryption::decrypt_with_ring(value, ring, field.table, field.column, id)
        .is_err()
    {
        return Classification::Invalid;
    }
    match database::field_encryption::envelope_key_id(value).and_then(|id| ring.canonical_id(id)) {
        Some(key_id) if key_id == ring.write_id() => Classification::Current,
        Some(key_id) => Classification::Stale(key_id.to_string()),
        None => Classification::Invalid,
    }
}

pub(super) fn rewrap_envelope(
    ring: &KeyRing,
    field: &Field,
    id: Uuid,
    value: &Value,
) -> anyhow::Result<String> {
    let plaintext = decrypt_ring_envelope(ring, field, id, value)?;
    envelope(ring.write_key(), ring.write_id(), field, id, &plaintext).context("rewrap failed")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Encrypt,
    Rewrap,
    Verify,
}

pub(super) fn job_action(
    mode: Mode,
    actions: &[String],
) -> Result<Action, (axum::http::StatusCode, axum::Json<Value>)> {
    match (mode, actions) {
        (Mode::Verify, [action]) if action == "verify" => Ok(Action::Verify),
        (Mode::DryRun | Mode::Execute, [action]) if action == "encrypt" => Ok(Action::Encrypt),
        (Mode::DryRun | Mode::Execute, [action]) if action == "rewrap" => Ok(Action::Rewrap),
        _ => Err(bad("actions do not match the requested job mode")),
    }
}

pub(super) fn validate_delay(
    delay: Option<i64>,
) -> Result<(), (axum::http::StatusCode, axum::Json<Value>)> {
    if delay.is_some_and(|value| !(0..=10_000).contains(&value)) {
        return Err(bad("batch_delay_ms must be between 0 and 10000"));
    }
    Ok(())
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct KeyCounts {
    pub totals: BTreeMap<String, i64>,
    pub fields: BTreeMap<String, BTreeMap<String, i64>>,
}

impl KeyCounts {
    pub fn retain_ring_keys(&mut self, ring: &KeyRing) {
        let canonicalize = |counts: &mut BTreeMap<String, i64>| {
            *counts = std::mem::take(counts)
                .into_iter()
                .filter_map(|(id, count)| ring.canonical_id(&id).map(|id| (id.to_owned(), count)))
                .collect();
        };
        canonicalize(&mut self.totals);
        self.fields.retain(|name, counts| {
            if !super::FIELDS
                .iter()
                .any(|field| *name == format!("{}.{}", field.table, field.column))
            {
                return false;
            }
            canonicalize(counts);
            !counts.is_empty()
        });
    }

    pub fn record(&mut self, field: &Field, key_id: Option<&str>) {
        if let Some(key_id) = key_id {
            *self.totals.entry(key_id.to_string()).or_default() += 1;
            *self
                .fields
                .entry(format!("{}.{}", field.table, field.column))
                .or_default()
                .entry(key_id.to_string())
                .or_default() += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Scope, FIELDS};
    use super::*;
    use serde_json::json;

    fn classify(ring: &KeyRing, field: &Field, id: Uuid, raw: &str) -> Classification {
        let value = serde_json::from_str(raw).unwrap_or(Value::Null);
        super::classify(ring, field, id, &value)
    }

    #[test]
    fn classification_checks_key_identity_authentication_and_context() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![("db-v1".into(), [7; 32])]).unwrap();
        let field = &FIELDS[0];
        let id = Uuid::new_v4();
        assert_eq!(
            classify(&ring, field, id, "plaintext"),
            Classification::Plaintext
        );
        let current = envelope(&[9; 32], "db-v2", field, id, "current").unwrap();
        assert_eq!(
            classify(&ring, field, id, &current),
            Classification::Current
        );
        let old = envelope(&[7; 32], "db-v1", field, id, "previous").unwrap();
        assert_eq!(
            classify(&ring, field, id, &old),
            Classification::Stale("db-v1".into())
        );
        assert_eq!(
            classify(&ring, field, Uuid::new_v4(), &old),
            Classification::Invalid
        );
        assert_eq!(
            classify(&ring, &FIELDS[1], id, &old),
            Classification::Invalid
        );
        let unknown = envelope(&[8; 32], "db-v3", field, id, "unknown").unwrap();
        assert_eq!(
            classify(&ring, field, id, &unknown),
            Classification::Invalid
        );
        let wrong_key = envelope(&[8; 32], "db-v2", field, id, "wrong").unwrap();
        assert_eq!(
            classify(&ring, field, id, &wrong_key),
            Classification::Invalid
        );
        assert_eq!(
            classify(&ring, field, id, &json!({MARKER:true}).to_string()),
            Classification::Invalid
        );
        assert_eq!(
            classify(&ring, field, id, &json!({MARKER:false}).to_string()),
            Classification::Plaintext
        );
        let old_value = serde_json::from_str(&old).unwrap();
        let rewritten = rewrap_envelope(&ring, field, id, &old_value).unwrap();
        let rewritten_value = serde_json::from_str(&rewritten).unwrap();
        let reduced = KeyRing::new("db-v2".into(), [9; 32], vec![]).unwrap();
        assert_eq!(
            classify(&reduced, field, id, &rewritten),
            Classification::Current
        );
        assert_eq!(
            decrypt_ring_envelope(&reduced, field, id, &rewritten_value).unwrap(),
            "previous"
        );
        let mut counts = KeyCounts::default();
        counts.record(field, classify(&ring, field, id, &old).key_id(&ring));
        counts.record(field, classify(&ring, field, id, &rewritten).key_id(&ring));
        let restored: KeyCounts =
            serde_json::from_value(serde_json::to_value(counts).unwrap()).unwrap();
        assert_eq!(restored.totals["db-v1"], 1);
        assert_eq!(restored.fields["response_items.item"]["db-v2"], 1);
    }

    #[test]
    fn action_mode_matrix_is_exact() {
        for mode in [Mode::DryRun, Mode::Execute, Mode::Verify] {
            for action in ["encrypt", "rewrap", "verify", "unknown"] {
                let expected = match mode {
                    Mode::Verify => action == "verify",
                    Mode::DryRun | Mode::Execute => matches!(action, "encrypt" | "rewrap"),
                };
                assert_eq!(job_action(mode, &[action.into()]).is_ok(), expected);
                assert!(job_action(mode, &[action.into(), action.into()]).is_err());
            }
            assert!(job_action(mode, &[]).is_err());
        }
    }

    #[test]
    fn forged_envelope_with_real_key_id_is_not_counted() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![]).unwrap();
        let field = &FIELDS[0];
        let id = Uuid::new_v4();
        let mut forged: Value = serde_json::from_str(
            &envelope(ring.write_key(), ring.write_id(), field, id, "secret").unwrap(),
        )
        .unwrap();
        forged["ciphertext"] = json!("garbage");
        let raw = forged.to_string();
        let classification = classify(&ring, field, id, &raw);
        assert_eq!(classification, Classification::Invalid);

        let mut counts = KeyCounts::default();
        counts.record(field, classification.key_id(&ring));

        assert!(counts.totals.is_empty());
        assert!(counts.fields.is_empty());
    }

    #[test]
    fn arbitrary_invalid_marker_key_id_is_not_counted() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![]).unwrap();
        let field = &FIELDS[0];
        let value = json!({MARKER:true, "version":1, "alg":"AES-256-GCM",
            "key_id": "invalid key/".repeat(10_000), "nonce":"", "ciphertext":""});
        let classification = super::classify(&ring, field, Uuid::new_v4(), &value);
        assert_eq!(classification, Classification::Invalid);
        let mut counts = KeyCounts::default();
        counts.record(field, classification.key_id(&ring));
        assert!(counts.totals.is_empty());
        assert!(counts.fields.is_empty());
    }

    #[test]
    fn stale_attribution_uses_canonical_ring_ids() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![("db-v1".into(), [7; 32])]).unwrap();
        let stale = Classification::Stale("db-v1".into());
        let id = stale.key_id(&ring).unwrap();
        let canonical = ring.read_ids()[1];
        assert!(std::ptr::eq(id, canonical));
        assert_eq!(Classification::Stale("unknown".into()).key_id(&ring), None);
    }

    #[test]
    fn restored_key_counts_only_retain_ring_ids_and_allowed_fields() {
        let ring = KeyRing::new("db-v2".into(), [9; 32], vec![("db-v1".into(), [7; 32])]).unwrap();
        let field = format!("{}.{}", FIELDS[0].table, FIELDS[0].column);
        let invalid_id = "invalid key/".repeat(10_000);
        let mut counts: KeyCounts = serde_json::from_value(json!({
            "totals": {"db-v1":2, "db-v2":1, (invalid_id.clone()):9},
            "fields": {(field.clone()): {"db-v1":2, "db-v2":1, (invalid_id):9},
                "unknown.field": {"db-v2":7}}
        }))
        .unwrap();

        counts.retain_ring_keys(&ring);

        assert_eq!(counts.totals.len(), 2);
        assert_eq!(counts.fields.len(), 1);
        assert_eq!(
            serde_json::to_value(counts).unwrap(),
            json!({
                "totals":{"db-v1":2,"db-v2":1},
                "fields":{(field):{"db-v1":2,"db-v2":1}}
            })
        );
    }

    #[tokio::test]
    async fn duplicate_delay_sources_are_rejected_before_database_access() {
        let pool = database::DbPool::uninitialized();
        pool.set_encryption_ring(KeyRing::new("db-v2".into(), [9; 32], vec![]).unwrap());
        let state = super::super::DatabaseEncryptionState::new(pool).unwrap();
        let admin = super::super::AdminUser(database::User {
            id: Uuid::nil(),
            email: "admin@example.com".into(),
            username: "admin".into(),
            display_name: None,
            avatar_url: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_login_at: None,
            is_active: true,
            auth_provider: "google".into(),
            provider_user_id: "admin".into(),
            tokens_revoked_at: None,
            contact_email: None,
            contact_email_verified_at: None,
            email_opt_in_at: None,
        });
        let request = serde_json::from_value(json!({
            "mode":"dry_run", "actions":["rewrap"], "batch_delay_ms":5,
            "scope":{"fields":[{"table":"files","column":"filename"}],
                     "options":{"batch_delay_ms":10}}
        }))
        .unwrap();

        let result = super::super::create_job(
            axum::Extension(state),
            axum::Extension(admin),
            axum::Json(request),
        )
        .await;

        let (status, body) = result.expect_err("duplicate delay request rejected");
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"]["message"],
            "specify batch_delay_ms at the top level or in scope.options, not both"
        );
    }

    #[test]
    fn rewrap_errors_have_a_distinct_safe_class() {
        assert_eq!(
            super::super::classify_job_error(&anyhow::anyhow!("rewrap failed")),
            "rewrap_failed"
        );
    }

    #[test]
    fn delay_bounds_and_scope_options_round_trip() {
        for delay in [None, Some(0), Some(1), Some(10_000)] {
            assert!(validate_delay(delay).is_ok());
        }
        for delay in [-1, 10_001, i64::MAX] {
            let (status, body) = validate_delay(Some(delay)).unwrap_err();
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
            assert_eq!(
                body["error"]["message"],
                "batch_delay_ms must be between 0 and 10000"
            );
        }
        let mut scope: Scope = serde_json::from_value(json!({"fields":[{"table":"files","column":"filename"}], "options":{"batch_delay_ms":5}})).unwrap();
        super::super::normalize_scope(&mut scope);
        assert_eq!(super::super::selected(&scope).unwrap().len(), 1);
        let restored: Scope = serde_json::from_value(serde_json::to_value(scope).unwrap()).unwrap();
        assert_eq!(restored.options.batch_delay_ms, Some(5));
    }
}
