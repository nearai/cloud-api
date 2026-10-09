//! Decide which observed rows may be pinned.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use services::attestation::tinfoil_pins::{ModelPin, RouterPin, TinfoilPins};

use crate::sigstore_verify::SigstoreResult;

pub const ROUTER_REPO: &str = "tinfoilsh/confidential-model-router";

/// The Sigstore predicate type whose `snp_measurement` is the router's SEV-SNP
/// launch measurement. A differently shaped predicate may use that field name
/// for something else, so router rows are accepted only for this type.
pub const ROUTER_PREDICATE_TYPE: &str = "https://tinfoil.sh/predicate/snp-tdx-multiplatform/v1";

/// The router as seen live: `observe()` returned Ok for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterObservation {
    pub measurement_hex: String,
    pub spki_sha256_hex: String,
    pub tcb: String,
    pub format: String,
}

/// A model entry as `/.well-known/tinfoil-proxy` publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelObservation {
    pub slug: String,
    pub repo: String,
    pub tag: String,
    pub kind: String,
    pub registers: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observations {
    pub observed_at: String,
    pub router: Option<RouterObservation>,
    pub models: Vec<ModelObservation>,
}

/// Verified Sigstore attestations: the router's, and each model release by
/// `repo@tag`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigstoreResults {
    pub router: Option<SigstoreResult>,
    pub models: BTreeMap<String, SigstoreResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// Seen and verified in this run.
    Live,
    /// Verified by an earlier run on `main` (its audit artifact).
    Earlier,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Added {
    /// `None` for the router.
    pub slug: Option<String>,
    pub repo: String,
    pub tag: String,
    pub measurement: Vec<String>,
    pub observed_at: String,
    pub source: Source,
}

/// Rows verified by earlier runs (their audit artifacts), with the time each
/// was first observed, so a report can say when an earlier row was seen rather
/// than claim it was seen now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Earlier {
    pub pins: TinfoilPins,
    observed_at: BTreeMap<Row, String>,
}

impl Earlier {
    /// Rows without a recorded observation time (reported as `unknown`).
    pub fn from_pins(pins: TinfoilPins) -> Self {
        let mut e = Self::default();
        e.merge(&pins, "unknown");
        e
    }

    /// Add `verified`, observed at `observed_at`. A row keeps the earliest
    /// time it was recorded with; a known time replaces `unknown`.
    pub fn merge(&mut self, verified: &TinfoilPins, observed_at: &str) {
        self.pins.merge_never_remove(verified);
        for row in rows(verified) {
            let slot = self
                .observed_at
                .entry(row)
                .or_insert_with(|| observed_at.to_string());
            if *slot == "unknown" || (observed_at != "unknown" && observed_at < slot.as_str()) {
                *slot = observed_at.to_string();
            }
        }
    }

    fn observed_at(&self, row: &Row) -> String {
        self.observed_at
            .get(row)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string())
    }
}

pub fn classify(
    base: &TinfoilPins,
    observed: &Observations,
    sigstore: &SigstoreResults,
) -> (TinfoilPins, Vec<Added>) {
    classify_with_earlier(base, &Earlier::default(), observed, sigstore)
}

/// One pin row, router or model.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Row {
    Router(RouterPin),
    Model(String, ModelPin),
}

fn rows(p: &TinfoilPins) -> Vec<Row> {
    let routers = p.router.iter().cloned().map(Row::Router);
    let models = p
        .models
        .iter()
        .flat_map(|(slug, ps)| ps.iter().map(|m| Row::Model(slug.clone(), m.clone())));
    routers.chain(models).collect()
}

fn has(p: &TinfoilPins, row: &Row) -> bool {
    match row {
        Row::Router(r) => p.router.contains(r),
        Row::Model(slug, m) => p.models.get(slug).is_some_and(|ms| ms.contains(m)),
    }
}

/// Rows seen in this run whose measurements equal the verified Sigstore
/// predicate, and a note for each observed row whose Sigstore result exists but
/// disagrees with it (categories only: that disagreement is the anomaly a
/// reviewer must see).
fn evaluate(observed: &Observations, sigstore: &SigstoreResults) -> (TinfoilPins, Vec<String>) {
    let mut live = TinfoilPins::default();
    let mut rejected = Vec::new();
    if let (Some(o), Some(s)) = (&observed.router, &sigstore.router) {
        if s.repo != ROUTER_REPO {
            rejected.push("router: sigstore repo is not the router repo".to_string());
        } else if s.predicate_type != ROUTER_PREDICATE_TYPE {
            rejected.push("router: unexpected sigstore predicate type".to_string());
        } else if !o.measurement_hex.eq_ignore_ascii_case(&s.snp_measurement) {
            rejected.push("router: live measurement differs from sigstore predicate".to_string());
        } else {
            live.router.push(RouterPin {
                measurement: o.measurement_hex.to_ascii_lowercase(),
                repo: ROUTER_REPO.to_string(),
                tag: s.tag.clone(),
            });
        }
    }
    for m in &observed.models {
        let release = format!("{}@{}", m.repo, m.tag);
        let Some(s) = sigstore.models.get(&release) else {
            continue;
        };
        let equal = s.repo == m.repo
            && s.tag == m.tag
            && s.predicate_type == m.kind
            && s.registers().is_some_and(|r| {
                r.len() == m.registers.len()
                    && r.iter()
                        .zip(&m.registers)
                        .all(|(a, b)| a.eq_ignore_ascii_case(b))
            });
        if equal {
            live.models
                .entry(m.slug.clone())
                .or_default()
                .push(ModelPin {
                    registers: m.registers.iter().map(|r| r.to_ascii_lowercase()).collect(),
                    repo: m.repo.clone(),
                    tag: m.tag.clone(),
                });
        } else {
            rejected.push(format!(
                "model {}: live registers differ from sigstore predicate for {release}",
                m.slug
            ));
        }
    }
    (live, rejected)
}

/// Why observed rows were not pinned although their release has a verified
/// Sigstore attestation. Append to the run's notes.
pub fn rejections(observed: &Observations, sigstore: &SigstoreResults) -> Vec<String> {
    evaluate(observed, sigstore).1
}

/// `earlier` holds rows verified by earlier runs on `main` (their audit
/// artifacts); they are added the same way live rows are. A row is live only if
/// it was seen in this run and equals the verified Sigstore predicate. `Added`
/// lists the rows not already in `base`. Rows are never removed.
pub fn classify_with_earlier(
    base: &TinfoilPins,
    earlier: &Earlier,
    observed: &Observations,
    sigstore: &SigstoreResults,
) -> (TinfoilPins, Vec<Added>) {
    let (live, _) = evaluate(observed, sigstore);
    let mut pins = base.clone();
    pins.merge_never_remove(&live);
    pins.merge_never_remove(&earlier.pins);

    let mut added: Vec<Added> = Vec::new();
    let candidates = rows(&live).into_iter().map(|r| (r, Source::Live)).chain(
        rows(&earlier.pins)
            .into_iter()
            .map(|r| (r, Source::Earlier)),
    );
    let mut seen = Vec::new();
    for (row, source) in candidates {
        if has(base, &row) || seen.contains(&row) {
            continue;
        }
        let observed_at = match source {
            Source::Live => observed.observed_at.clone(),
            Source::Earlier => earlier.observed_at(&row),
        };
        added.push(match &row {
            Row::Router(r) => Added {
                slug: None,
                repo: r.repo.clone(),
                tag: r.tag.clone(),
                measurement: vec![r.measurement.clone()],
                observed_at: observed_at.clone(),
                source,
            },
            Row::Model(slug, m) => Added {
                slug: Some(slug.clone()),
                repo: m.repo.clone(),
                tag: m.tag.clone(),
                measurement: m.registers.clone(),
                observed_at: observed_at.clone(),
                source,
            },
        });
        seen.push(row);
    }
    (pins, added)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "aa";
    const M2: &str = "bb";

    fn sig(m: &str) -> SigstoreResult {
        SigstoreResult {
            repo: ROUTER_REPO.into(),
            tag: "v1".into(),
            predicate_type: ROUTER_PREDICATE_TYPE.into(),
            subject_sha256: "d".into(),
            snp_measurement: m.into(),
            rtmr1: Some("r1".into()),
            rtmr2: Some("r2".into()),
        }
    }
    fn obs_router(m: &str) -> Observations {
        Observations {
            observed_at: "t".into(),
            router: Some(RouterObservation {
                measurement_hex: m.into(),
                spki_sha256_hex: "s".into(),
                tcb: "1/0/1/1".into(),
                format: "f".into(),
            }),
            models: vec![],
        }
    }
    fn router_sig(m: &str) -> SigstoreResults {
        SigstoreResults {
            router: Some(sig(m)),
            models: BTreeMap::new(),
        }
    }
    fn rp(m: &str) -> RouterPin {
        RouterPin {
            measurement: m.into(),
            repo: ROUTER_REPO.into(),
            tag: "v1".into(),
        }
    }
    fn model_obs(regs: &[&str]) -> Observations {
        Observations {
            observed_at: "t".into(),
            router: None,
            models: vec![ModelObservation {
                slug: "m".into(),
                repo: "tinfoilsh/x".into(),
                tag: "v1".into(),
                kind: "p".into(),
                registers: regs.iter().map(|s| s.to_string()).collect(),
            }],
        }
    }
    fn model_sig(snp: &str) -> SigstoreResults {
        let mut r = sig(snp);
        r.repo = "tinfoilsh/x".into();
        r.predicate_type = "p".into();
        SigstoreResults {
            router: None,
            models: BTreeMap::from([("tinfoilsh/x@v1".to_string(), r)]),
        }
    }

    #[test]
    fn classify_adds_router_row_only_when_sigstore_equal() {
        let (p, added) = classify(&TinfoilPins::default(), &obs_router(M), &router_sig(M));
        assert_eq!(p.router, vec![rp(M)]);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].slug, None);
        assert_eq!(added[0].source, Source::Live);

        let (p, added) = classify(&TinfoilPins::default(), &obs_router(M), &router_sig(M2));
        assert!(p.router.is_empty() && added.is_empty());
    }

    #[test]
    fn classify_requires_a_live_observation_and_a_sigstore_result() {
        let none = Observations::default();
        let (p, _) = classify(&TinfoilPins::default(), &none, &router_sig(M));
        assert!(p.router.is_empty());
        let (p, _) = classify(
            &TinfoilPins::default(),
            &obs_router(M),
            &SigstoreResults::default(),
        );
        assert!(p.router.is_empty());
    }

    #[test]
    fn classify_never_removes_rows() {
        let base = TinfoilPins {
            router: vec![rp("old")],
            models: BTreeMap::from([(
                "gone".to_string(),
                vec![ModelPin {
                    registers: vec!["x".into()],
                    repo: "r".into(),
                    tag: "t".into(),
                }],
            )]),
        };
        let (p, added) = classify(&base, &obs_router(M), &router_sig(M));
        assert!(p.router.contains(&rp("old")) && p.router.contains(&rp(M)));
        assert!(p.models.contains_key("gone"));
        assert_eq!(added.len(), 1);
    }

    #[test]
    fn already_pinned_rows_are_not_reported_as_added() {
        let base = TinfoilPins {
            router: vec![rp(M)],
            models: BTreeMap::new(),
        };
        let (p, added) = classify(&base, &obs_router(M), &router_sig(M));
        assert_eq!(p, base);
        assert!(added.is_empty());
    }

    #[test]
    fn model_row_needs_all_registers_equal_to_the_sigstore_predicate() {
        let (p, added) = classify(
            &TinfoilPins::default(),
            &model_obs(&["aa", "r1", "r2"]),
            &model_sig("aa"),
        );
        assert_eq!(p.models["m"][0].registers, vec!["aa", "r1", "r2"]);
        assert_eq!(added[0].slug.as_deref(), Some("m"));

        for regs in [["bb", "r1", "r2"], ["aa", "r1", "zz"]] {
            let (p, added) = classify(&TinfoilPins::default(), &model_obs(&regs), &model_sig("aa"));
            assert!(p.models.is_empty() && added.is_empty());
        }
    }

    #[test]
    fn model_row_needs_matching_tag_and_predicate_type() {
        let mut s = model_sig("aa");
        s.models.get_mut("tinfoilsh/x@v1").unwrap().tag = "v2".into();
        let (p, _) = classify(&TinfoilPins::default(), &model_obs(&["aa", "r1", "r2"]), &s);
        assert!(p.models.is_empty());
        let mut s = model_sig("aa");
        s.models.get_mut("tinfoilsh/x@v1").unwrap().predicate_type = "other".into();
        let (p, _) = classify(&TinfoilPins::default(), &model_obs(&["aa", "r1", "r2"]), &s);
        assert!(p.models.is_empty());
    }

    #[test]
    fn earlier_verified_rows_are_added_and_marked_earlier() {
        let mut earlier = Earlier::default();
        earlier.merge(
            &TinfoilPins {
                router: vec![rp("prev")],
                models: BTreeMap::new(),
            },
            "2026-10-01T06:30:00Z",
        );
        let (p, added) = classify_with_earlier(
            &TinfoilPins::default(),
            &earlier,
            &Observations {
                observed_at: "2026-10-07T06:30:00Z".into(),
                ..Observations::default()
            },
            &SigstoreResults::default(),
        );
        assert_eq!(p.router, vec![rp("prev")]);
        assert_eq!(added[0].source, Source::Earlier);
        // The report says when the earlier run saw the row, not this run's time.
        assert_eq!(added[0].observed_at, "2026-10-01T06:30:00Z");
    }

    #[test]
    fn earlier_row_keeps_its_earliest_known_time() {
        let pins = TinfoilPins {
            router: vec![rp("prev")],
            models: BTreeMap::new(),
        };
        let mut e = Earlier::default();
        e.merge(&pins, "2026-10-05T00:00:00Z");
        e.merge(&pins, "2026-10-03T00:00:00Z");
        e.merge(&pins, "2026-10-06T00:00:00Z");
        let (_, added) = classify_with_earlier(
            &TinfoilPins::default(),
            &e,
            &Observations::default(),
            &SigstoreResults::default(),
        );
        assert_eq!(added[0].observed_at, "2026-10-03T00:00:00Z");
        let (_, added) = classify_with_earlier(
            &TinfoilPins::default(),
            &Earlier::from_pins(pins),
            &Observations::default(),
            &SigstoreResults::default(),
        );
        assert_eq!(added[0].observed_at, "unknown");
    }

    #[test]
    fn router_row_needs_the_router_predicate_type() {
        let mut s = router_sig(M);
        s.router.as_mut().unwrap().predicate_type = "https://example.test/other/v1".into();
        let (p, added) = classify(&TinfoilPins::default(), &obs_router(M), &s);
        assert!(p.router.is_empty() && added.is_empty());
        assert_eq!(
            rejections(&obs_router(M), &s),
            vec!["router: unexpected sigstore predicate type"]
        );
    }

    #[test]
    fn disagreements_with_the_sigstore_predicate_are_reported() {
        assert_eq!(
            rejections(&obs_router(M), &router_sig(M2)),
            vec!["router: live measurement differs from sigstore predicate"]
        );
        assert_eq!(
            rejections(&model_obs(&["bb", "r1", "r2"]), &model_sig("aa")),
            vec!["model m: live registers differ from sigstore predicate for tinfoilsh/x@v1"]
        );
        // Agreement, and rows with no Sigstore result at all (noted by the
        // probe), produce no rejection.
        assert!(rejections(&obs_router(M), &router_sig(M)).is_empty());
        assert!(rejections(&model_obs(&["aa", "r1", "r2"]), &model_sig("aa")).is_empty());
        assert!(rejections(&model_obs(&["aa"]), &SigstoreResults::default()).is_empty());
    }
}
