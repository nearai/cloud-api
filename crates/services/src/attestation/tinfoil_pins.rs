//! The compiled allow-list of Tinfoil router and model measurements.
//!
//! Rows live in `tinfoil_golden_measurements.json` (compiled in with
//! `include_str!`). Rows are only ever added: see [`TinfoilPins::merge_never_remove`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const COMPILED_PINS_JSON: &str = include_str!("tinfoil_golden_measurements.json");

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TinfoilPins {
    pub router: Vec<RouterPin>,
    pub models: BTreeMap<String, Vec<ModelPin>>,
}

/// SEV-SNP launch measurement (96 hex chars) of a router release.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterPin {
    pub measurement: String,
    pub repo: String,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPin {
    pub registers: Vec<String>,
    pub repo: String,
    pub tag: String,
}

impl TinfoilPins {
    fn canonicalise(&mut self) {
        self.router.sort();
        self.router.dedup();
        for pins in self.models.values_mut() {
            pins.sort();
            pins.dedup();
        }
        self.models.retain(|_, v| !v.is_empty());
    }

    /// Add every row of `other` that is not already present. Never removes.
    pub fn merge_never_remove(&mut self, other: &TinfoilPins) {
        for r in &other.router {
            if !self.router.contains(r) {
                self.router.push(r.clone());
            }
        }
        for (slug, pins) in &other.models {
            let mine = self.models.entry(slug.clone()).or_default();
            for p in pins {
                if !mine.contains(p) {
                    mine.push(p.clone());
                }
            }
        }
        self.canonicalise();
    }

    /// Canonical file contents: sorted, 2-space pretty JSON, trailing newline.
    pub fn to_canonical_json(&self) -> String {
        let mut c = self.clone();
        c.canonicalise();
        let mut s = serde_json::to_string_pretty(&c).expect("TinfoilPins always serialises");
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(m: &str) -> RouterPin {
        RouterPin {
            measurement: m.into(),
            repo: "r".into(),
            tag: "t".into(),
        }
    }
    fn mp(r: &str) -> ModelPin {
        ModelPin {
            registers: vec![r.into()],
            repo: "r".into(),
            tag: "t".into(),
        }
    }

    #[test]
    fn canonical_json_round_trips() {
        let p: TinfoilPins =
            serde_json::from_str(include_str!("testdata/tinfoil/test_pins.json")).unwrap();
        let s = p.to_canonical_json();
        assert_eq!(serde_json::from_str::<TinfoilPins>(&s).unwrap(), p);
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn merge_never_removes_rows() {
        let mut base = TinfoilPins {
            router: vec![rp("a")],
            models: BTreeMap::from([("m".to_string(), vec![mp("a")])]),
        };
        let other = TinfoilPins {
            router: vec![rp("b")],
            models: BTreeMap::from([
                ("m".to_string(), vec![mp("b")]),
                ("n".to_string(), vec![mp("c")]),
            ]),
        };
        base.merge_never_remove(&other);
        assert_eq!(base.router, vec![rp("a"), rp("b")]);
        assert_eq!(base.models["m"], vec![mp("a"), mp("b")]);
        assert_eq!(base.models["n"], vec![mp("c")]);
        let before = base.clone();
        base.merge_never_remove(&TinfoilPins::default());
        assert_eq!(base, before);
    }
}
