//! The `tinfoil_trust` runtime setting, owned by Tinfoil attestation. It is
//! an [`admin_settings`](crate::admin_settings) key: admins change it with
//! `PATCH /v1/admin/settings/tinfoil_trust`, and Tinfoil reads it through a
//! [`Versioned`] handle.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::admin_settings::{SettingKey, Versioned};

/// The effective Tinfoil trust switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TinfoilTrust {
    /// Trust a Tinfoil router from its Sigstore bundle at verify time.
    /// Default true.
    pub router_sigstore: bool,
}

impl Default for TinfoilTrust {
    fn default() -> Self {
        Self {
            router_sigstore: true,
        }
    }
}

pub struct TinfoilTrustKey;

impl SettingKey for TinfoilTrustKey {
    const NAME: &'static str = "tinfoil_trust";
    const FIELDS: &'static [&'static str] = &["router_sigstore"];
    type Value = TinfoilTrust;
    type Published = Versioned<TinfoilTrust>;

    fn default_value() -> TinfoilTrust {
        TinfoilTrust::default()
    }

    /// Strict (PATCH): unknown fields and wrong types are errors. Lenient
    /// (stored row): unknown fields (from a newer binary) are ignored. An
    /// absent `router_sigstore` uses its default (true); a wrong-typed one
    /// fails closed (false), since it is a kill switch. Only the field name
    /// is logged.
    fn parse(value: &Value, strict: bool) -> Result<TinfoilTrust, String> {
        if strict {
            return serde_json::from_value(value.clone()).map_err(|e| e.to_string());
        }
        let mut trust = TinfoilTrust::default();
        if let Some(v) = value.get("router_sigstore") {
            match v.as_bool() {
                Some(b) => trust.router_sigstore = b,
                None => {
                    // Fail closed: a corrupt kill-switch row must not re-enable Sigstore.
                    trust.router_sigstore = false;
                    tracing::warn!(
                        setting = Self::NAME,
                        field = "router_sigstore",
                        "Stored admin setting field has the wrong type; failing closed"
                    );
                }
            }
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
        // Absent falls back to the default (true), never errors.
        assert!(
            TinfoilTrustKey::parse(&json!({"future": 1}), false)
                .unwrap()
                .router_sigstore
        );
        // Wrong-typed fails closed: a kill-switch row we cannot read must not
        // re-enable Sigstore admission.
        assert!(
            !TinfoilTrustKey::parse(&json!({"router_sigstore": "no"}), false)
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
        assert_eq!(v.value, json!({"router_sigstore": false}));
        assert!(!handle.load().value.router_sigstore);
        assert_eq!(handle.load().version, 1);
        assert!(!svc.current::<TinfoilTrustKey>().router_sigstore);

        let got = svc.get("tinfoil_trust").await.unwrap();
        assert_eq!(got.value, json!({"router_sigstore": false}));
        svc.reload().await;
        assert!(!handle.load().value.router_sigstore);
        assert_eq!(handle.load().version, 1, "reload of the same row");

        let v = svc
            .update("tinfoil_trust", json!({"router_sigstore": null}), admin)
            .await
            .unwrap();
        assert_eq!(v.value, json!({"router_sigstore": true}));
        assert!(handle.load().value.router_sigstore);
    }
}
