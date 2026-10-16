//! The `tinfoil_trust` runtime setting, owned by Tinfoil attestation. It is
//! an [`admin_settings`](crate::admin_settings) key: admins change it with
//! `PATCH /v1/admin/settings/tinfoil_trust`, and Tinfoil reads it through a
//! [`Versioned`] handle.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::admin_settings::{SettingKey, Versioned};

/// Most model pins one `tinfoil_trust` value may carry.
pub const MAX_MODEL_PINS: usize = 32;

/// The only repository owner whose releases may vouch for a model.
const MODEL_REPO_PREFIX: &str = "tinfoilsh/";

/// One admin-added model pin: Tinfoil's Sigstore release bundle for `repo` at
/// `tag`, admitted for the router slug `slug`. The bundle is verified offline
/// at PATCH and again by the verifier of record when it is loaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminModelPin {
    pub slug: String,
    pub repo: String,
    pub tag: String,
    pub sigstore_bundle: Value,
}

/// The effective Tinfoil trust switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TinfoilTrust {
    /// Trust a Tinfoil router from its Sigstore bundle at verify time.
    /// Default true.
    pub router_sigstore: bool,
    /// Admit models from `model_pins`. Default true.
    pub admin_model_pins: bool,
    /// Admin-added, Sigstore-verified model pins (at most 32).
    pub model_pins: Vec<AdminModelPin>,
}

impl Default for TinfoilTrust {
    fn default() -> Self {
        Self {
            router_sigstore: true,
            admin_model_pins: true,
            model_pins: Vec::new(),
        }
    }
}

pub struct TinfoilTrustKey;

/// Strict (PATCH) checks of one pin. Messages carry the slug and the
/// verifier's reason only, never bundle content.
fn verify_pin_strict(pin: &AdminModelPin) -> Result<(), String> {
    if !pin.repo.starts_with(MODEL_REPO_PREFIX) {
        return Err(format!(
            "model pin {}: repo must be a tinfoilsh repository",
            pin.slug
        ));
    }
    let prov = tinfoil_provenance::verify_release(&pin.sigstore_bundle, &pin.repo)
        .map_err(|e| format!("model pin {}: {e}", pin.slug))?;
    if prov.tag != pin.tag {
        return Err(format!(
            "model pin {}: tag does not match the signed release",
            pin.slug
        ));
    }
    Ok(())
}

fn str_field<'a>(v: &'a Value, name: &str) -> &'a str {
    v.get(name).and_then(Value::as_str).unwrap_or("")
}

/// Structural read of a stored pin: its four fields, unknown fields ignored.
fn lenient_pin(raw: &Value) -> Option<AdminModelPin> {
    Some(AdminModelPin {
        slug: raw.get("slug")?.as_str()?.to_string(),
        repo: raw.get("repo")?.as_str()?.to_string(),
        tag: raw.get("tag")?.as_str()?.to_string(),
        sigstore_bundle: raw
            .get("sigstore_bundle")
            .filter(|b| b.is_object())?
            .clone(),
    })
}

impl SettingKey for TinfoilTrustKey {
    const NAME: &'static str = "tinfoil_trust";
    const FIELDS: &'static [&'static str] = &["router_sigstore", "admin_model_pins", "model_pins"];
    const MAX_BYTES: usize = 1 << 20;
    type Value = TinfoilTrust;
    type Published = Versioned<TinfoilTrust>;

    fn default_value() -> TinfoilTrust {
        TinfoilTrust::default()
    }

    /// Strict (PATCH): unknown fields and wrong types are errors, at most 32
    /// pins, and every pin's Sigstore bundle must verify for its repo at its
    /// tag. Lenient (stored row): structural only, no crypto. Unknown fields
    /// (from a newer binary) are ignored, a switch of the wrong type falls
    /// back to its default, and a pin that does not deserialize is dropped
    /// (more than 32 keep the first 32). Only field names, slug, repo, tag
    /// and counts are logged.
    fn parse(value: &Value, strict: bool) -> Result<TinfoilTrust, String> {
        if strict {
            let trust: TinfoilTrust =
                serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            if trust.model_pins.len() > MAX_MODEL_PINS {
                return Err(format!("model_pins accepts at most {MAX_MODEL_PINS} pins"));
            }
            for (i, pin) in trust.model_pins.iter().enumerate() {
                if trust.model_pins[..i]
                    .iter()
                    .any(|p| p.slug == pin.slug && p.repo == pin.repo && p.tag == pin.tag)
                {
                    return Err(format!("model pin {}: duplicate pin", pin.slug));
                }
                verify_pin_strict(pin)?;
            }
            return Ok(trust);
        }
        let mut trust = TinfoilTrust::default();
        for (field, slot) in [
            ("router_sigstore", &mut trust.router_sigstore),
            ("admin_model_pins", &mut trust.admin_model_pins),
        ] {
            if let Some(v) = value.get(field) {
                match v.as_bool() {
                    Some(b) => *slot = b,
                    None => tracing::warn!(
                        setting = Self::NAME,
                        field,
                        "Stored admin setting field has the wrong type; using its default"
                    ),
                }
            }
        }
        match value.get("model_pins") {
            Some(Value::Array(pins)) => {
                if pins.len() > MAX_MODEL_PINS {
                    tracing::warn!(
                        setting = Self::NAME,
                        count = pins.len(),
                        "Stored tinfoil_trust has too many model pins; keeping the first {MAX_MODEL_PINS}"
                    );
                }
                for raw in pins.iter().take(MAX_MODEL_PINS) {
                    match lenient_pin(raw) {
                        Some(pin) => trust.model_pins.push(pin),
                        None => tracing::warn!(
                            slug = str_field(raw, "slug"),
                            repo = str_field(raw, "repo"),
                            tag = str_field(raw, "tag"),
                            "Stored tinfoil_trust model pin is malformed; ignoring it"
                        ),
                    }
                }
            }
            Some(_) => tracing::warn!(
                setting = Self::NAME,
                field = "model_pins",
                "Stored admin setting field has the wrong type; using its default"
            ),
            None => {}
        }
        Ok(trust)
    }

    fn publish(value: &TinfoilTrust, version: u64) -> Versioned<TinfoilTrust> {
        Versioned {
            version,
            value: value.clone(),
        }
    }
}

/// A pin built from the recorded real model release bundle.
#[cfg(test)]
pub(crate) const RECORDED_MODEL_REPO: &str = "tinfoilsh/confidential-deepseek-v4-1-flash";

#[cfg(test)]
pub(crate) fn recorded_model_pin(slug: &str) -> AdminModelPin {
    let bundle: Value = serde_json::from_str(include_str!(
        "../../../tinfoil_provenance/testdata/model_sigstore_bundle.json"
    ))
    .unwrap();
    let prov = tinfoil_provenance::verify_release(&bundle, RECORDED_MODEL_REPO).unwrap();
    AdminModelPin {
        slug: slug.to_string(),
        repo: RECORDED_MODEL_REPO.to_string(),
        tag: prov.tag,
        sigstore_bundle: bundle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_settings::{AdminSettingsRepository, AdminSettingsService, StoredSetting};
    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[derive(Default)]
    struct FakeRepo {
        rows: Mutex<Vec<StoredSetting>>,
    }

    #[async_trait]
    impl AdminSettingsRepository for FakeRepo {
        async fn get_all(&self) -> anyhow::Result<Vec<StoredSetting>> {
            Ok(self.rows.lock().unwrap().clone())
        }

        async fn get(&self, key: &str) -> anyhow::Result<Option<StoredSetting>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.key == key)
                .cloned())
        }

        async fn upsert(
            &self,
            key: &str,
            value: Value,
            by_user: Uuid,
        ) -> anyhow::Result<StoredSetting> {
            let row = StoredSetting {
                key: key.to_string(),
                value,
                updated_by_user_id: Some(by_user),
                updated_at: chrono::Utc::now(),
            };
            let mut rows = self.rows.lock().unwrap();
            rows.retain(|r| r.key != key);
            rows.push(row.clone());
            Ok(row)
        }
    }

    #[test]
    fn default_is_router_sigstore_on() {
        assert!(TinfoilTrust::default().router_sigstore);
        assert!(TinfoilTrustKey::default_value().router_sigstore);
        assert!(
            TinfoilTrustKey::parse(&json!({}), true)
                .unwrap()
                .router_sigstore
        );
    }

    #[test]
    fn strict_rejects_unknown_field() {
        assert!(TinfoilTrustKey::parse(&json!({"future": 1}), true).is_err());
        assert!(TinfoilTrustKey::parse(&json!({"router_sigstore": "no"}), true).is_err());
    }

    #[test]
    fn lenient_ignores_unknown_fields_and_keeps_router_sigstore() {
        let t =
            TinfoilTrustKey::parse(&json!({"router_sigstore": false, "future": 1}), false).unwrap();
        assert!(!t.router_sigstore);
        // Absent or wrong-typed falls back to the default, never errors.
        assert!(
            TinfoilTrustKey::parse(&json!({"future": 1}), false)
                .unwrap()
                .router_sigstore
        );
        assert!(
            TinfoilTrustKey::parse(&json!({"router_sigstore": "no"}), false)
                .unwrap()
                .router_sigstore
        );
    }

    #[tokio::test]
    async fn patch_router_sigstore_false_roundtrips() {
        let repo = Arc::new(FakeRepo::default());
        let handle = Arc::new(ArcSwap::from_pointee(Versioned {
            version: 0,
            value: TinfoilTrust::default(),
        }));
        let svc = AdminSettingsService::builder(repo.clone())
            .register::<TinfoilTrustKey>(handle.clone())
            .build();
        let admin = Uuid::new_v4();
        let v = svc
            .update("tinfoil_trust", json!({"router_sigstore": false}), admin)
            .await
            .unwrap();
        assert_eq!(v.value, trust_json(false));
        assert!(!handle.load().value.router_sigstore);
        assert_eq!(handle.load().version, 1);
        assert!(!svc.current::<TinfoilTrustKey>().router_sigstore);

        let got = svc.get("tinfoil_trust").await.unwrap();
        assert_eq!(got.value, trust_json(false));
        svc.reload().await;
        assert!(!handle.load().value.router_sigstore);
        assert_eq!(handle.load().version, 1, "reload of the same row");

        let v = svc
            .update("tinfoil_trust", json!({"router_sigstore": null}), admin)
            .await
            .unwrap();
        assert_eq!(v.value, trust_json(true));
        assert!(handle.load().value.router_sigstore);
    }

    fn trust_json(router_sigstore: bool) -> Value {
        json!({"router_sigstore": router_sigstore, "admin_model_pins": true, "model_pins": []})
    }

    fn pin_json(pin: &AdminModelPin) -> Value {
        serde_json::to_value(pin).unwrap()
    }

    fn strict_err(pins: Vec<Value>) -> String {
        TinfoilTrustKey::parse(&json!({"model_pins": pins}), true).unwrap_err()
    }

    #[test]
    fn valid_recorded_model_bundle_accepted() {
        let pin = recorded_model_pin("deepseek-v4-1-flash");
        let t = TinfoilTrustKey::parse(&json!({"model_pins": [pin_json(&pin)]}), true).unwrap();
        assert_eq!(t.model_pins, vec![pin]);
        assert!(t.admin_model_pins);
    }

    #[test]
    fn over_32_pins_rejected() {
        let pin = pin_json(&recorded_model_pin("s"));
        let err = strict_err(vec![pin; 33]);
        assert!(err.contains("at most 32"), "{err}");
    }

    #[test]
    fn over_1mib_rejected() {
        assert_eq!(TinfoilTrustKey::MAX_BYTES, 1 << 20);
        let repo = Arc::new(FakeRepo::default());
        let handle = Arc::new(ArcSwap::from_pointee(Versioned {
            version: 0,
            value: TinfoilTrust::default(),
        }));
        let svc = AdminSettingsService::builder(repo)
            .register::<TinfoilTrustKey>(handle)
            .build();
        let mut pin = pin_json(&recorded_model_pin("s"));
        pin["sigstore_bundle"]["pad"] = json!("x".repeat(1 << 20));
        let r = futures::executor::block_on(svc.update(
            "tinfoil_trust",
            json!({"model_pins": [pin]}),
            Uuid::new_v4(),
        ));
        assert!(matches!(
            r,
            Err(crate::admin_settings::AdminSettingsError::Invalid(m))
                if m == "the setting value is too large"
        ));
    }

    #[test]
    fn invalid_bundle_rejected_with_reason() {
        let mut pin = recorded_model_pin("my-slug");
        pin.sigstore_bundle = json!({"secret_marker_xyz": 1});
        let err = strict_err(vec![pin_json(&pin)]);
        assert!(err.starts_with("model pin my-slug: sigstore: "), "{err}");
        assert!(!err.contains("secret_marker_xyz"), "{err}");
    }

    #[test]
    fn tag_mismatch_rejected() {
        let mut pin = recorded_model_pin("my-slug");
        pin.tag = "v9.9.9".into();
        assert_eq!(
            strict_err(vec![pin_json(&pin)]),
            "model pin my-slug: tag does not match the signed release"
        );
    }

    #[test]
    fn repo_not_tinfoilsh_rejected() {
        let mut pin = recorded_model_pin("my-slug");
        pin.repo = "evil/confidential-deepseek-v4-1-flash".into();
        assert_eq!(
            strict_err(vec![pin_json(&pin)]),
            "model pin my-slug: repo must be a tinfoilsh repository"
        );
    }

    #[test]
    fn wrong_repo_for_bundle_rejected_and_duplicates_rejected() {
        let mut pin = recorded_model_pin("my-slug");
        pin.repo = "tinfoilsh/other".into();
        assert!(strict_err(vec![pin_json(&pin)]).starts_with("model pin my-slug: "));
        let ok = pin_json(&recorded_model_pin("my-slug"));
        assert_eq!(
            strict_err(vec![ok.clone(), ok]),
            "model pin my-slug: duplicate pin"
        );
    }

    #[test]
    fn strict_rejects_unknown_pin_field() {
        let mut pin = pin_json(&recorded_model_pin("s"));
        pin["extra"] = json!(1);
        assert!(TinfoilTrustKey::parse(&json!({"model_pins": [pin]}), true).is_err());
    }

    #[test]
    fn stored_invalid_pin_dropped_not_fatal() {
        let good = recorded_model_pin("good");
        let stored = json!({
            "router_sigstore": false,
            "admin_model_pins": false,
            "future": 1,
            "model_pins": [
                {"slug": "bad", "repo": "tinfoilsh/x", "tag": "v1"},
                {"slug": 5},
                "junk",
                {"slug": "future", "repo": "tinfoilsh/y", "tag": "v2",
                 "sigstore_bundle": {"a": 1}, "newer_field": true},
                pin_json(&good),
            ]
        });
        let t = TinfoilTrustKey::parse(&stored, false).unwrap();
        assert!(!t.router_sigstore);
        assert!(!t.admin_model_pins);
        let slugs: Vec<&str> = t.model_pins.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(slugs, ["future", "good"]);
    }

    #[test]
    fn stored_more_than_32_pins_keeps_first_32() {
        let pin = pin_json(&recorded_model_pin("s"));
        let t = TinfoilTrustKey::parse(&json!({"model_pins": vec![pin; 40]}), false).unwrap();
        assert_eq!(t.model_pins.len(), 32);
    }
}
