//! The compiled allow-list of Chutes golden measurements.
//!
//! The rows live in `chutes_golden_measurements.json`, compiled into the binary
//! with `include_str!`, so the reproducible build still commits to exactly which
//! Chutes software identities cloud-api accepts. The daily
//! `chutes-measurements-sync` workflow rewrites this file with
//! [`PinsFile::to_canonical_json`]; keep it in that form.
//!
//! JSON has no comments, so the evidence for each row lives elsewhere: rows
//! pinned before the file existed are documented in the history of
//! `vetted_golden_measurements()` in `chutes.rs` (`git log -p` up to #1192);
//! rows added by the sync job are documented in that job's PR body (instance,
//! model and date seen) and its 90-day run artifact (raw quotes and nonces).

use inference_providers::attested::chutes::measurements::{
    is_register_hex, normalize_register, ChutesMeasurementPolicy, ExpectedMeasurement,
};
use serde::{Deserialize, Serialize};

pub const COMPILED_PINS_JSON: &str = include_str!("chutes_golden_measurements.json");

/// The five pinned TDX registers as lowercase hex (no `0x`). `rtmr3` is the
/// runtime value (`runtime_rtmrs.RTMR3`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Registers {
    pub mrtd: String,
    pub rtmr0: String,
    pub rtmr1: String,
    pub rtmr2: String,
    pub rtmr3: String,
}

impl Registers {
    pub fn normalised(self) -> Self {
        Self {
            mrtd: normalize_register(&self.mrtd),
            rtmr0: normalize_register(&self.rtmr0),
            rtmr1: normalize_register(&self.rtmr1),
            rtmr2: normalize_register(&self.rtmr2),
            rtmr3: normalize_register(&self.rtmr3),
        }
    }

    /// Every register is exactly 48 bytes of hex.
    pub fn is_well_formed(&self) -> bool {
        [
            &self.mrtd,
            &self.rtmr0,
            &self.rtmr1,
            &self.rtmr2,
            &self.rtmr3,
        ]
        .iter()
        .all(|h| is_register_hex(h))
    }

    /// An all-zero runtime RTMR3 means the running app is unmeasured (v1.0–v1.2
    /// boot templates). Such rows are never pinned.
    pub fn rtmr3_is_zero(&self) -> bool {
        !self.rtmr3.is_empty() && self.rtmr3.bytes().all(|b| b == b'0')
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinsFile {
    pub families: Vec<PinFamily>,
}

/// One software release: MRTD + RTMR1/2 + runtime RTMR3 are shared by every
/// hardware row; only RTMR0 varies per hardware/VM sizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinFamily {
    pub version: String,
    pub mrtd: String,
    pub rtmr1: String,
    pub rtmr2: String,
    pub rtmr3: String,
    pub hardware_rows: Vec<PinRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinRow {
    pub name: String,
    pub rtmr0: String,
}

impl PinFamily {
    pub fn registers_for(&self, row: &PinRow) -> Registers {
        Registers {
            mrtd: self.mrtd.clone(),
            rtmr0: row.rtmr0.clone(),
            rtmr1: self.rtmr1.clone(),
            rtmr2: self.rtmr2.clone(),
            rtmr3: self.rtmr3.clone(),
        }
    }

    /// Same software identity (everything but RTMR0).
    pub fn same_identity(&self, r: &Registers) -> bool {
        self.mrtd == r.mrtd
            && self.rtmr1 == r.rtmr1
            && self.rtmr2 == r.rtmr2
            && self.rtmr3 == r.rtmr3
    }
}

impl PinsFile {
    /// Parse a pins file, normalising every register to lowercase hex without
    /// `0x` so comparisons with feed and live values are exact.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let mut pins: Self = serde_json::from_str(json)?;
        for f in &mut pins.families {
            f.mrtd = normalize_register(&f.mrtd);
            f.rtmr1 = normalize_register(&f.rtmr1);
            f.rtmr2 = normalize_register(&f.rtmr2);
            f.rtmr3 = normalize_register(&f.rtmr3);
            for r in &mut f.hardware_rows {
                r.rtmr0 = normalize_register(&r.rtmr0);
            }
        }
        Ok(pins)
    }

    /// The compiled-in pins. `compiled_pins_parse_and_are_enforceable` rules
    /// out a parse error in CI; callers still handle it rather than panic.
    pub fn compiled() -> Result<Self, serde_json::Error> {
        Self::parse(COMPILED_PINS_JSON)
    }

    pub fn rows(&self) -> impl Iterator<Item = (&PinFamily, &PinRow)> {
        self.families
            .iter()
            .flat_map(|f| f.hardware_rows.iter().map(move |r| (f, r)))
    }

    /// The pinned row whose five registers equal `r`, if any.
    pub fn find(&self, r: &Registers) -> Option<(&PinFamily, &PinRow)> {
        self.rows()
            .find(|(f, row)| f.same_identity(r) && row.rtmr0 == r.rtmr0)
    }

    pub fn to_policy(&self) -> ChutesMeasurementPolicy {
        ChutesMeasurementPolicy::new(
            self.rows()
                .map(|(f, r)| {
                    ExpectedMeasurement::new(
                        r.name.clone(),
                        f.version.clone(),
                        &f.mrtd,
                        &r.rtmr0,
                        &f.rtmr1,
                        &f.rtmr2,
                        &f.rtmr3,
                    )
                })
                .collect(),
        )
    }

    /// Families stable-sorted by `version` string; rows sorted by `name`.
    pub fn canonicalise(&mut self) {
        self.families.sort_by(|a, b| a.version.cmp(&b.version));
        for f in &mut self.families {
            f.hardware_rows.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    /// Canonical file contents: canonical order, 2-space pretty JSON, trailing
    /// newline.
    pub fn to_canonical_json(&self) -> String {
        let mut c = self.clone();
        c.canonicalise();
        let mut s = serde_json::to_string_pretty(&c).expect("PinsFile always serialises");
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO: &str = "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn compiled_pins_parse_and_are_enforceable() {
        let pins = PinsFile::compiled().expect("compiled pins parse");
        assert!(!pins.families.is_empty());
        pins.to_policy()
            .assert_enforceable()
            .expect("every pinned register must be 48-byte hex");
    }

    #[test]
    fn compiled_pins_file_is_canonical() {
        // The sync bot writes `to_canonical_json()`; the checked-in file must
        // already be in that form so bot diffs only show added rows.
        assert_eq!(
            PinsFile::compiled()
                .expect("compiled pins parse")
                .to_canonical_json(),
            COMPILED_PINS_JSON
        );
    }

    #[test]
    fn compiled_pins_have_no_zero_runtime_rtmr3() {
        for f in &PinsFile::compiled().expect("compiled pins parse").families {
            assert_ne!(f.rtmr3, ZERO, "v{} pins an unmeasured runtime", f.version);
        }
    }

    #[test]
    fn compiled_pins_have_unique_names_and_identities() {
        let pins = PinsFile::compiled().expect("compiled pins parse");
        let mut idents = std::collections::HashSet::new();
        for f in &pins.families {
            assert!(
                idents.insert((&f.version, &f.mrtd, &f.rtmr1, &f.rtmr2, &f.rtmr3)),
                "duplicate family v{}",
                f.version
            );
            let mut names = std::collections::HashSet::new();
            for r in &f.hardware_rows {
                assert!(
                    names.insert(&r.name),
                    "duplicate row {} in v{}",
                    r.name,
                    f.version
                );
            }
        }
    }

    #[test]
    fn registers_are_normalised_to_lowercase_without_prefix() {
        let r = Registers {
            mrtd: "0xAB".into(),
            rtmr0: " CD ".into(),
            rtmr1: "ef".into(),
            rtmr2: "0X01".into(),
            rtmr3: "23".into(),
        }
        .normalised();
        assert_eq!(
            (r.mrtd.as_str(), r.rtmr0.as_str(), r.rtmr2.as_str()),
            ("ab", "cd", "01")
        );
    }

    #[test]
    fn zero_rtmr3_is_detected() {
        let mut r = Registers {
            mrtd: "ab".into(),
            rtmr0: "ab".into(),
            rtmr1: "ab".into(),
            rtmr2: "ab".into(),
            rtmr3: ZERO.into(),
        };
        assert!(r.rtmr3_is_zero());
        r.rtmr3 = "0001".into();
        assert!(!r.rtmr3_is_zero());
    }

    #[test]
    fn parse_normalises_registers() {
        // A hand-edited or carried file may use uppercase or 0x; comparisons
        // with feed and live values (lowercase, no prefix) must still match.
        let up = "AB".repeat(48);
        let json = format!(
            r#"{{"families":[{{"version":"1.4.1","mrtd":"0x{up}","rtmr1":"{up}","rtmr2":"{up}","rtmr3":"{up}","hardware_rows":[{{"name":"x","rtmr0":"0X{up}"}}]}}]}}"#
        );
        let pins = PinsFile::parse(&json).unwrap();
        let low = "ab".repeat(48);
        assert_eq!(pins.families[0].mrtd, low);
        assert_eq!(pins.families[0].rtmr3, low);
        assert_eq!(pins.families[0].hardware_rows[0].rtmr0, low);
    }

    #[test]
    fn find_requires_all_five_registers() {
        let pins = PinsFile::compiled().expect("compiled pins parse");
        let (fam, row) = pins.rows().next().unwrap();
        let exact = fam.registers_for(row);
        assert!(pins.find(&exact).is_some());
        let mut off = exact.clone();
        off.rtmr2 = ZERO.to_string();
        assert!(pins.find(&off).is_none());
    }

    #[test]
    fn canonicalise_sorts_rows_and_keeps_equal_version_order() {
        let fam = |version: &str, rtmr3: &str, rows: &[(&str, &str)]| PinFamily {
            version: version.into(),
            mrtd: "a".into(),
            rtmr1: "b".into(),
            rtmr2: "c".into(),
            rtmr3: rtmr3.into(),
            hardware_rows: rows
                .iter()
                .map(|(n, r)| PinRow {
                    name: (*n).into(),
                    rtmr0: (*r).into(),
                })
                .collect(),
        };
        let mut pins = PinsFile {
            families: vec![
                fam("1.4.1", "d", &[("z", "1"), ("a", "2")]),
                fam("1.3.0", "e", &[]),
                fam("1.4.1", "f", &[]),
            ],
        };
        pins.canonicalise();
        assert_eq!(pins.families[0].version, "1.3.0");
        assert_eq!(pins.families[1].hardware_rows[0].name, "a");
        assert_eq!(
            pins.families[2].rtmr3, "f",
            "equal versions keep their order"
        );
    }
}
