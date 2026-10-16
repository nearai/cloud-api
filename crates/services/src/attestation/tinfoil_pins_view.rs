//! The public view of which Tinfoil trust inputs are live: the compiled pins,
//! and the admin-added model pins with their Sigstore provenance. Served by
//! `GET /v1/attestation/pins` (the `tinfoil` section). Raw Sigstore bundles are never part of
//! the view (size); only provenance fields are.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use tinfoil_provenance::Provenance;

use crate::admin_settings::AdminSettingsService;

use super::pins_view::ProviderPinsSource;
use super::tinfoil::compiled_tinfoil_pins;
use super::tinfoil_pins::{ModelPin, RouterPin, TinfoilPins};
use super::tinfoil_trust::{verify_admin_pin_bundle, AdminModelPin, TinfoilTrust, TinfoilTrustKey};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompiledRouterView {
    pub measurement: String,
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompiledModelView {
    pub repo: String,
    pub tag: String,
    pub registers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompiledPinsView {
    pub routers: Vec<CompiledRouterView>,
    pub models: BTreeMap<String, Vec<CompiledModelView>>,
}

/// One admin pin. Provenance fields are present only when `verified`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AdminPinView {
    pub slug: String,
    pub repo: String,
    pub tag: String,
    pub rekor_log_index: Option<u64>,
    pub integrated_time: Option<String>,
    pub snp_measurement: Option<String>,
    pub rtmr1: Option<String>,
    pub rtmr2: Option<String>,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AdminPinsView {
    pub router_sigstore: bool,
    pub admin_model_pins: bool,
    pub model_pins: Vec<AdminPinView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TinfoilPinsView {
    pub compiled: CompiledPinsView,
    pub runtime: AdminPinsView,
}

/// The shared admission check; an unverified pin carries no provenance.
fn verify_pin(pin: &AdminModelPin) -> Option<Provenance> {
    verify_admin_pin_bundle(pin).ok()
}

fn admin_pin_view(pin: &AdminModelPin) -> AdminPinView {
    let prov = verify_pin(pin);
    AdminPinView {
        slug: pin.slug.clone(),
        repo: pin.repo.clone(),
        tag: pin.tag.clone(),
        rekor_log_index: prov.as_ref().map(|p| p.rekor_log_index),
        integrated_time: prov.as_ref().map(|p| p.integrated_time.to_rfc3339()),
        snp_measurement: prov.as_ref().map(|p| p.snp_measurement.clone()),
        rtmr1: prov.as_ref().and_then(|p| p.rtmr1.clone()),
        rtmr2: prov.as_ref().and_then(|p| p.rtmr2.clone()),
        verified: prov.is_some(),
    }
}

/// Build the public view from the compiled pins and the effective trust.
pub fn tinfoil_pins_view(pins: &TinfoilPins, trust: &TinfoilTrust) -> TinfoilPinsView {
    TinfoilPinsView {
        compiled: CompiledPinsView {
            routers: pins
                .router
                .iter()
                .map(
                    |RouterPin {
                         measurement, tag, ..
                     }| CompiledRouterView {
                        measurement: measurement.clone(),
                        tag: tag.clone(),
                    },
                )
                .collect(),
            models: pins
                .models
                .iter()
                .map(|(slug, ps)| {
                    (
                        slug.clone(),
                        ps.iter()
                            .map(
                                |ModelPin {
                                     registers,
                                     repo,
                                     tag,
                                 }| CompiledModelView {
                                    repo: repo.clone(),
                                    tag: tag.clone(),
                                    registers: registers.clone(),
                                },
                            )
                            .collect(),
                    )
                })
                .collect(),
        },
        runtime: AdminPinsView {
            router_sigstore: trust.router_sigstore,
            admin_model_pins: trust.admin_model_pins,
            model_pins: trust.model_pins.iter().map(admin_pin_view).collect(),
        },
    }
}

/// Tinfoil's section of the public pins view: compiled pins plus the runtime
/// `tinfoil_trust` setting, versioned by that setting.
pub struct TinfoilPinsSource {
    settings: Arc<AdminSettingsService>,
}

impl TinfoilPinsSource {
    pub fn new(settings: Arc<AdminSettingsService>) -> Self {
        Self { settings }
    }
}

impl ProviderPinsSource for TinfoilPinsSource {
    fn provider(&self) -> &'static str {
        "tinfoil"
    }

    fn version(&self) -> u64 {
        self.settings.version::<TinfoilTrustKey>()
    }

    fn public_view(&self) -> serde_json::Value {
        let Ok(pins) = compiled_tinfoil_pins() else {
            tracing::error!("tinfoil compiled pins unavailable for the public pins view");
            return serde_json::json!({"error": "compiled pins unavailable"});
        };
        // Version 0 means no stored value has loaded yet: the serving
        // verifier is on its fail-closed initial trust, so report that rather
        // than the registry's defaults. Read after `version()`, so the view is
        // never older than its version.
        let trust = if self.version() == 0 {
            TinfoilTrust::fail_closed_initial()
        } else {
            self.settings.current::<TinfoilTrustKey>()
        };
        serde_json::to_value(tinfoil_pins_view(pins, &trust)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attestation::tinfoil::vetted_tinfoil_pins;

    fn recorded_pin() -> AdminModelPin {
        let bundle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tinfoil_provenance/testdata/model_sigstore_bundle.json"
        ))
        .unwrap();
        let repo = "tinfoilsh/confidential-deepseek-v4-1-flash".to_string();
        let tag = "v0.0.3".to_string();
        AdminModelPin {
            slug: "deepseek-v4-1-flash".into(),
            repo,
            tag,
            sigstore_bundle: bundle,
        }
    }

    #[test]
    fn compiled_pins_are_listed_and_admin_defaults_are_on() {
        let pins = vetted_tinfoil_pins().unwrap();
        let v = tinfoil_pins_view(&pins, &TinfoilTrust::default());
        assert_eq!(v.compiled.routers.len(), pins.router.len());
        assert_eq!(v.compiled.models.len(), pins.models.len());
        assert!(v.runtime.router_sigstore && v.runtime.admin_model_pins);
        assert!(v.runtime.model_pins.is_empty());
    }

    #[test]
    fn recorded_bundle_pin_is_verified_with_provenance() {
        let pin = recorded_pin();
        let trust = TinfoilTrust {
            model_pins: vec![pin.clone()],
            ..Default::default()
        };
        let v = tinfoil_pins_view(&TinfoilPins::default(), &trust);
        let p = &v.runtime.model_pins[0];
        assert!(p.verified, "recorded bundle must verify");
        assert_eq!(p.tag, pin.tag);
        assert!(p.rekor_log_index.is_some());
        assert!(p.integrated_time.is_some());
        assert!(p.snp_measurement.is_some() && p.rtmr1.is_some() && p.rtmr2.is_some());
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.contains("sigstore_bundle"), "bundle is never served");
    }

    #[test]
    fn tag_mismatch_pin_is_unverified() {
        let mut pin = recorded_pin();
        pin.tag = "v9.9.9".into();
        let trust = TinfoilTrust {
            model_pins: vec![pin],
            ..Default::default()
        };
        let v = tinfoil_pins_view(&TinfoilPins::default(), &trust);
        let p = &v.runtime.model_pins[0];
        assert!(!p.verified);
        assert!(p.rekor_log_index.is_none() && p.snp_measurement.is_none());
    }

    #[test]
    fn non_tinfoilsh_repo_is_unverified() {
        let mut pin = recorded_pin();
        pin.repo = "evil/confidential-deepseek-v4-1-flash".into();
        let trust = TinfoilTrust {
            model_pins: vec![pin],
            ..Default::default()
        };
        let v = tinfoil_pins_view(&TinfoilPins::default(), &trust);
        let p = &v.runtime.model_pins[0];
        assert!(!p.verified);
        assert!(p.rekor_log_index.is_none() && p.snp_measurement.is_none());
    }

    #[test]
    fn garbage_bundle_pin_is_unverified_without_provenance() {
        let trust = TinfoilTrust {
            model_pins: vec![AdminModelPin {
                slug: "m".into(),
                repo: "tinfoilsh/confidential-m".into(),
                tag: "v1".into(),
                sigstore_bundle: serde_json::json!({"garbage": true}),
            }],
            ..Default::default()
        };
        let v = tinfoil_pins_view(&TinfoilPins::default(), &trust);
        let p = &v.runtime.model_pins[0];
        assert!(!p.verified);
        assert!(p.rekor_log_index.is_none() && p.snp_measurement.is_none());
    }

    #[tokio::test]
    async fn runtime_switches_are_fail_closed_until_the_first_load() {
        use crate::admin_settings::{AdminSettingsRepository, StoredSetting};
        use arc_swap::ArcSwap;

        struct EmptyRepo;
        #[async_trait::async_trait]
        impl AdminSettingsRepository for EmptyRepo {
            async fn get_all(&self) -> anyhow::Result<Vec<StoredSetting>> {
                Ok(Vec::new())
            }
            async fn get(&self, _key: &str) -> anyhow::Result<Option<StoredSetting>> {
                Ok(None)
            }
            async fn upsert(
                &self,
                _key: &str,
                _value: serde_json::Value,
                _by_user: uuid::Uuid,
            ) -> anyhow::Result<StoredSetting> {
                Err(anyhow::anyhow!("read-only"))
            }
        }

        let handle = Arc::new(ArcSwap::from_pointee(crate::admin_settings::Versioned {
            version: 0,
            value: TinfoilTrust::fail_closed_initial(),
        }));
        let settings = Arc::new(
            AdminSettingsService::builder(Arc::new(EmptyRepo))
                .register::<TinfoilTrustKey>(handle)
                .build(),
        );
        let source = TinfoilPinsSource::new(settings.clone());
        // Before any load the serving verifier has both paths off, and so
        // must the public view.
        assert_eq!(source.version(), 0);
        let before = source.public_view();
        assert_eq!(before["runtime"]["router_sigstore"], false);
        assert_eq!(before["runtime"]["admin_model_pins"], false);
        // After the first load the defaults (both on) are live.
        assert!(settings.reload().await);
        assert_eq!(source.version(), 1);
        let after = source.public_view();
        assert_eq!(after["runtime"]["router_sigstore"], true);
        assert_eq!(after["runtime"]["admin_model_pins"], true);
    }
}
