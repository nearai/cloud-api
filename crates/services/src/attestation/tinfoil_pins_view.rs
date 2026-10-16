//! The public view of which Tinfoil trust inputs are live: the compiled pins,
//! and the admin-added model pins with their Sigstore provenance. Served by
//! `GET /v1/attestation/tinfoil/pins`. Raw Sigstore bundles are never part of
//! the view (size); only provenance fields are.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tinfoil_provenance::Provenance;

use super::tinfoil_pins::{ModelPin, RouterPin, TinfoilPins};
use super::tinfoil_trust::{verify_admin_pin_bundle, AdminModelPin, TinfoilTrust};

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
    pub admin: AdminPinsView,
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
        admin: AdminPinsView {
            router_sigstore: trust.router_sigstore,
            admin_model_pins: trust.admin_model_pins,
            model_pins: trust.model_pins.iter().map(admin_pin_view).collect(),
        },
    }
}

/// The rendered pins response body for the latest `tinfoil_trust` version
/// seen. The anonymous route checks the version first, so a hit costs one
/// atomic read and an `Arc` clone: no copy of the trust value, no hashing,
/// no verification.
#[derive(Default)]
pub struct PinsViewMemo {
    slot: Mutex<Option<(u64, Arc<serde_json::Value>)>>,
}

impl PinsViewMemo {
    /// The body rendered for exactly `version`, if memoized.
    pub fn get(&self, version: u64) -> Option<Arc<serde_json::Value>> {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref()
            .filter(|(v, _)| *v == version)
            .map(|(_, body)| body.clone())
    }

    /// Memoize `body` for `version`. A body for an older version never
    /// replaces a newer one.
    pub fn put(&self, version: u64, body: Arc<serde_json::Value>) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_none_or(|(v, _)| *v <= version) {
            *slot = Some((version, body));
        }
    }

    /// The memoized body for `version`, or `build` it (outside the lock) and
    /// memoize it.
    pub fn get_or_build(
        &self,
        version: u64,
        build: impl FnOnce() -> serde_json::Value,
    ) -> Arc<serde_json::Value> {
        if let Some(hit) = self.get(version) {
            return hit;
        }
        let body = Arc::new(build());
        self.put(version, body.clone());
        body
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
        assert!(v.admin.router_sigstore && v.admin.admin_model_pins);
        assert!(v.admin.model_pins.is_empty());
    }

    #[test]
    fn recorded_bundle_pin_is_verified_with_provenance() {
        let pin = recorded_pin();
        let trust = TinfoilTrust {
            model_pins: vec![pin.clone()],
            ..Default::default()
        };
        let v = tinfoil_pins_view(&TinfoilPins::default(), &trust);
        let p = &v.admin.model_pins[0];
        assert!(p.verified, "recorded bundle must verify");
        assert_eq!(p.tag, pin.tag);
        assert!(p.rekor_log_index.is_some());
        assert!(p.integrated_time.is_some());
        assert!(p.snp_measurement.is_some() && p.rtmr1.is_some() && p.rtmr2.is_some());
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.contains("sigstore_bundle"), "bundle is never served");
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
        let p = &v.admin.model_pins[0];
        assert!(!p.verified);
        assert!(p.rekor_log_index.is_none() && p.snp_measurement.is_none());
    }

    #[test]
    fn memo_builds_once_per_version() {
        use std::cell::Cell;
        let memo = PinsViewMemo::default();
        let builds = Cell::new(0);
        let build = || {
            builds.set(builds.get() + 1);
            let trust = TinfoilTrust {
                model_pins: vec![recorded_pin()],
                ..Default::default()
            };
            serde_json::to_value(tinfoil_pins_view(&TinfoilPins::default(), &trust)).unwrap()
        };
        let a = memo.get_or_build(1, build);
        let b = memo.get_or_build(1, build);
        assert_eq!(builds.get(), 1);
        assert!(Arc::ptr_eq(&a, &b));
        let c = memo.get_or_build(2, build);
        assert_eq!(builds.get(), 2, "a version bump rebuilds");
        assert!(!Arc::ptr_eq(&a, &c));
        memo.put(1, a);
        assert!(
            memo.get(1).is_none(),
            "an older body never replaces a newer one"
        );
        assert!(memo.get(2).is_some());
    }
}
