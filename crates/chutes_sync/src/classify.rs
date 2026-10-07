//! Decide which published measurement rows to pin, given the published feed
//! and register sets from verified live quotes.
//!
//! Rule: a row is added only if all five registers equal one published feed
//! row AND a quote that passed the quote, report_data and GPU checks showed
//! them. Rows are never removed; a row with an all-zero runtime RTMR3 is never
//! added.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use services::attestation::chutes_pins::{PinFamily, PinRow, PinsFile, Registers};

/// One row of `GET https://api.chutes.ai/servers/tee/measurements`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedRow {
    pub version: String,
    pub name: String,
    pub registers: Registers,
}

#[derive(Deserialize)]
struct RawFeedRow {
    version: String,
    name: String,
    #[serde(default)]
    mrtd: Option<String>,
    #[serde(default)]
    runtime_rtmrs: Option<RawRtmrs>,
}

#[derive(Deserialize)]
struct RawRtmrs {
    #[serde(rename = "RTMR0", default)]
    rtmr0: Option<String>,
    #[serde(rename = "RTMR1", default)]
    rtmr1: Option<String>,
    #[serde(rename = "RTMR2", default)]
    rtmr2: Option<String>,
    #[serde(rename = "RTMR3", default)]
    rtmr3: Option<String>,
}

/// Feed names and versions are written into the pins file and the PR body, and
/// the feed is unsigned: accept only the characters Chutes uses (letters,
/// digits, space and `._,-/+[]()`), up to 80 characters.
fn is_safe_label(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= 80
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._,-/+[]()".contains(c))
}

/// Parse the published feed. Uses `runtime_rtmrs` (RTMR3 is the runtime value).
/// Rows with missing or malformed registers, or an unexpected name or version,
/// are skipped.
pub fn parse_feed(json: &str) -> Result<Vec<FeedRow>, serde_json::Error> {
    let raw: Vec<RawFeedRow> = serde_json::from_str(json)?;
    Ok(raw
        .into_iter()
        .filter_map(|r| {
            let rt = r.runtime_rtmrs?;
            let registers = Registers {
                mrtd: r.mrtd?,
                rtmr0: rt.rtmr0?,
                rtmr1: rt.rtmr1?,
                rtmr2: rt.rtmr2?,
                rtmr3: rt.rtmr3?,
            }
            .normalised();
            (registers.is_well_formed() && is_safe_label(&r.version) && is_safe_label(&r.name))
                .then_some(FeedRow {
                    version: r.version,
                    name: r.name,
                    registers,
                })
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ObservationOutcome {
    Verified(Registers),
    Failed { stage: String },
}

/// What the probe saw for one instance.
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub model: String,
    pub chute_id: String,
    pub instance_id: String,
    pub outcome: ObservationOutcome,
}

/// A chute the probe could not read.
#[derive(Debug, Clone, Serialize)]
pub struct SkippedChute {
    pub model: String,
    pub chute_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RowRef {
    pub version: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeenOn {
    pub model: String,
    pub instance_id: String,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    /// Rows in the output that are not in `base` (today's additions plus
    /// carried rows from an unmerged bot PR), with where they were seen today.
    pub added: Vec<(RowRef, Vec<SeenOn>)>,
    /// Distinct verified register sets seen today that main already pins.
    pub already_pinned: usize,
    pub unpublished: Vec<(Registers, Vec<SeenOn>)>,
    pub unverified: Vec<Observation>,
    pub zero_rtmr3: Vec<RowRef>,
    pub identity_anomalies: Vec<RowRef>,
    /// Published, verified live, but the family already has a row with this
    /// name and a different RTMR0 (Chutes re-measured it). Needs a person.
    pub name_collisions: Vec<RowRef>,
    /// Chosen row, and the other published names for the same registers.
    pub aliases: Vec<(RowRef, Vec<String>)>,
    pub withdrawn_before_merge: Vec<RowRef>,
    pub pinned_not_published: Vec<RowRef>,
    pub published_not_observed: Vec<RowRef>,
    pub skipped: Vec<SkippedChute>,
}

/// Why a row could not be inserted.
enum InsertError {
    /// The version already has a family with a different software identity.
    IdentityAnomaly,
    /// The family already has a row with this name and a different RTMR0.
    NameCollision,
}

/// Insert a row into the family with the same version and software identity,
/// creating the family if that version has none. On error `pins` is untouched.
fn insert_row(
    pins: &mut PinsFile,
    version: &str,
    name: &str,
    regs: &Registers,
) -> Result<(), InsertError> {
    let row = PinRow {
        name: name.to_string(),
        rtmr0: regs.rtmr0.clone(),
    };
    if let Some(f) = pins
        .families
        .iter_mut()
        .find(|f| f.version == version && f.same_identity(regs))
    {
        if f.hardware_rows.iter().any(|r| r.name == name) {
            return Err(InsertError::NameCollision);
        }
        f.hardware_rows.push(row);
        return Ok(());
    }
    if pins.families.iter().any(|f| f.version == version) {
        return Err(InsertError::IdentityAnomaly);
    }
    pins.families.push(PinFamily {
        version: version.to_string(),
        mrtd: regs.mrtd.clone(),
        rtmr1: regs.rtmr1.clone(),
        rtmr2: regs.rtmr2.clone(),
        rtmr3: regs.rtmr3.clone(),
        hardware_rows: vec![row],
    });
    Ok(())
}

impl SyncReport {
    fn not_inserted(&mut self, err: InsertError, row: RowRef) {
        match err {
            InsertError::IdentityAnomaly => self.identity_anomalies.push(row),
            InsertError::NameCollision => self.name_collisions.push(row),
        }
    }
}

fn row_ref(version: &str, name: &str) -> RowRef {
    RowRef {
        version: version.to_string(),
        name: name.to_string(),
    }
}

/// Merge `base` (main's pins), `carry` (pins from the open bot PR) and today's
/// verified observations into the new pins file, and describe the result.
pub fn classify(
    feed: &[FeedRow],
    observations: &[Observation],
    skipped: Vec<SkippedChute>,
    base: &PinsFile,
    carry: Option<&PinsFile>,
) -> (PinsFile, SyncReport) {
    let mut report = SyncReport {
        skipped,
        ..Default::default()
    };

    // Published register sets -> (version, names). The same registers under
    // several names collapse to one entry; the smallest name is used.
    let mut published: BTreeMap<Registers, (String, BTreeSet<String>)> = BTreeMap::new();
    for row in feed {
        published
            .entry(row.registers.clone())
            .or_insert_with(|| (row.version.clone(), BTreeSet::new()))
            .1
            .insert(row.name.clone());
    }
    let chosen_name = |names: &BTreeSet<String>| names.iter().next().cloned().unwrap_or_default();

    let mut out = base.clone();

    // Pinned rows Chutes no longer publishes: kept, reported.
    for (f, r) in base.rows() {
        if !published.contains_key(&f.registers_for(r)) {
            report
                .pinned_not_published
                .push(row_ref(&f.version, &r.name));
        }
    }

    // Rows from an unmerged bot PR that main lacks: kept while still published,
    // otherwise dropped as withdrawn.
    if let Some(carry) = carry {
        for (f, r) in carry.rows() {
            let regs = f.registers_for(r);
            if base.find(&regs).is_some() {
                continue;
            }
            if !published.contains_key(&regs) || regs.rtmr3_is_zero() {
                report
                    .withdrawn_before_merge
                    .push(row_ref(&f.version, &r.name));
            } else if let Err(e) = insert_row(&mut out, &f.version, &r.name, &regs) {
                report.not_inserted(e, row_ref(&f.version, &r.name));
            }
        }
    }

    // Today's verified observations, grouped by register set.
    let mut seen: BTreeMap<Registers, BTreeSet<SeenOn>> = BTreeMap::new();
    for o in observations {
        match &o.outcome {
            ObservationOutcome::Verified(r) => {
                seen.entry(r.clone()).or_default().insert(SeenOn {
                    model: o.model.clone(),
                    instance_id: o.instance_id.clone(),
                });
            }
            ObservationOutcome::Failed { .. } => report.unverified.push(o.clone()),
        }
    }

    for (regs, on) in &seen {
        if base.find(regs).is_some() {
            report.already_pinned += 1;
            continue;
        }
        // Carried from the open bot PR: already in `out`, reported as added.
        if out.find(regs).is_some() {
            continue;
        }
        let Some((version, names)) = published.get(regs) else {
            report
                .unpublished
                .push((regs.clone(), on.iter().cloned().collect()));
            continue;
        };
        let row = row_ref(version, &chosen_name(names));
        if regs.rtmr3_is_zero() {
            report.zero_rtmr3.push(row);
            continue;
        }
        if let Err(e) = insert_row(&mut out, version, &row.name, regs) {
            report.not_inserted(e, row);
            continue;
        }
        if names.len() > 1 {
            report
                .aliases
                .push((row, names.iter().skip(1).cloned().collect()));
        }
    }

    out.canonicalise();

    for (f, r) in out.rows() {
        let regs = f.registers_for(r);
        if base.find(&regs).is_none() {
            let on = seen
                .get(&regs)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            report.added.push((row_ref(&f.version, &r.name), on));
        }
    }

    // Published rows neither pinned nor seen today (e.g. a future release).
    for (regs, (version, names)) in &published {
        if out.find(regs).is_none() && !seen.contains_key(regs) && !regs.rtmr3_is_zero() {
            report
                .published_not_observed
                .push(row_ref(version, &chosen_name(names)));
        }
    }

    for v in [
        &mut report.zero_rtmr3,
        &mut report.identity_anomalies,
        &mut report.name_collisions,
        &mut report.withdrawn_before_merge,
        &mut report.pinned_not_published,
        &mut report.published_not_observed,
    ] {
        v.sort();
        v.dedup();
    }
    report.added.sort();
    report.aliases.sort();
    (out, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::attestation::chutes_pins::{PinFamily, PinRow, PinsFile, Registers};

    fn h(c: char) -> String {
        std::iter::repeat_n(c, 96).collect()
    }
    fn regs(m: char, r0: char, r1: char, r2: char, r3: char) -> Registers {
        Registers {
            mrtd: h(m),
            rtmr0: h(r0),
            rtmr1: h(r1),
            rtmr2: h(r2),
            rtmr3: h(r3),
        }
    }
    fn feed(version: &str, name: &str, r: &Registers) -> FeedRow {
        FeedRow {
            version: version.into(),
            name: name.into(),
            registers: r.clone(),
        }
    }
    fn seen(model: &str, inst: &str, r: &Registers) -> Observation {
        Observation {
            model: model.into(),
            chute_id: format!("c-{model}"),
            instance_id: inst.into(),
            outcome: ObservationOutcome::Verified(r.clone()),
        }
    }
    fn pins_with(version: &str, name: &str, r: &Registers) -> PinsFile {
        PinsFile {
            families: vec![PinFamily {
                version: version.into(),
                mrtd: r.mrtd.clone(),
                rtmr1: r.rtmr1.clone(),
                rtmr2: r.rtmr2.clone(),
                rtmr3: r.rtmr3.clone(),
                hardware_rows: vec![PinRow {
                    name: name.into(),
                    rtmr0: r.rtmr0.clone(),
                }],
            }],
        }
    }
    fn empty() -> PinsFile {
        PinsFile { families: vec![] }
    }
    fn row(version: &str, name: &str) -> RowRef {
        RowRef {
            version: version.into(),
            name: name.into(),
        }
    }

    #[test]
    fn published_and_observed_row_is_added() {
        let r = regs('a', 'b', 'c', 'd', 'e');
        let (out, rep) = classify(
            &[feed("1.4.1", "8xb300", &r)],
            &[seen("kimi", "i1", &r)],
            vec![],
            &empty(),
            None,
        );
        assert!(out.find(&r).is_some());
        assert_eq!(rep.added.len(), 1);
        assert_eq!(rep.added[0].0, row("1.4.1", "8xb300"));
        assert_eq!(rep.added[0].1[0].model, "kimi");
    }

    #[test]
    fn partial_match_is_not_added() {
        let published = regs('a', 'b', 'c', 'd', 'e');
        let live = regs('a', 'b', 'c', 'd', 'f');
        let (out, rep) = classify(
            &[feed("1.4.1", "x", &published)],
            &[seen("m", "i", &live)],
            vec![],
            &empty(),
            None,
        );
        assert!(out.families.is_empty());
        assert_eq!(rep.unpublished.len(), 1);
        assert_eq!(rep.published_not_observed, vec![row("1.4.1", "x")]);
    }

    #[test]
    fn already_pinned_row_is_counted_not_duplicated() {
        let r = regs('a', 'b', 'c', 'd', 'e');
        let base = pins_with("1.4.1", "x", &r);
        let (out, rep) = classify(
            &[feed("1.4.1", "x", &r)],
            &[seen("m", "i", &r)],
            vec![],
            &base,
            None,
        );
        assert_eq!(out, base);
        assert_eq!(rep.already_pinned, 1);
        assert!(rep.added.is_empty());
    }

    #[test]
    fn zero_runtime_rtmr3_is_never_added() {
        let r = regs('a', 'b', 'c', 'd', '0');
        let (out, rep) = classify(
            &[feed("1.2.0", "x", &r)],
            &[seen("m", "i", &r)],
            vec![],
            &empty(),
            None,
        );
        assert!(out.families.is_empty());
        assert_eq!(rep.zero_rtmr3, vec![row("1.2.0", "x")]);
    }

    #[test]
    fn unverified_observation_is_reported_never_added() {
        let r = regs('a', 'b', 'c', 'd', 'e');
        let failed = Observation {
            model: "m".into(),
            chute_id: "c".into(),
            instance_id: "i".into(),
            outcome: ObservationOutcome::Failed {
                stage: "quote".into(),
            },
        };
        let (out, rep) = classify(&[feed("1.4.1", "x", &r)], &[failed], vec![], &empty(), None);
        assert!(out.families.is_empty());
        assert_eq!(rep.unverified.len(), 1);
    }

    #[test]
    fn one_register_set_under_two_names_becomes_one_row_with_smallest_name() {
        let r = regs('a', 'b', 'c', 'd', 'e');
        let (out, rep) = classify(
            &[feed("1.4.1", "zeta", &r), feed("1.4.1", "alpha", &r)],
            &[seen("m", "i", &r)],
            vec![],
            &empty(),
            None,
        );
        assert_eq!(out.families[0].hardware_rows.len(), 1);
        assert_eq!(out.families[0].hardware_rows[0].name, "alpha");
        assert_eq!(rep.aliases[0].1, vec!["zeta".to_string()]);
    }

    #[test]
    fn new_row_joins_family_with_same_identity() {
        let pinned = regs('a', 'b', 'c', 'd', 'e');
        let new = regs('a', '1', 'c', 'd', 'e');
        let base = pins_with("1.4.1", "old", &pinned);
        let (out, _) = classify(
            &[feed("1.4.1", "new", &new)],
            &[seen("m", "i", &new)],
            vec![],
            &base,
            None,
        );
        assert_eq!(out.families.len(), 1);
        assert_eq!(out.families[0].hardware_rows.len(), 2);
    }

    #[test]
    fn same_version_with_different_identity_is_an_anomaly() {
        let pinned = regs('a', 'b', 'c', 'd', 'e');
        let odd = regs('a', '1', 'c', '9', 'e');
        let base = pins_with("1.4.1", "old", &pinned);
        let (out, rep) = classify(
            &[feed("1.4.1", "odd", &odd)],
            &[seen("m", "i", &odd)],
            vec![],
            &base,
            None,
        );
        assert_eq!(out, base);
        assert_eq!(rep.identity_anomalies, vec![row("1.4.1", "odd")]);
    }

    #[test]
    fn name_already_used_in_family_is_reported_not_pinned() {
        // Chutes re-measured a row under the same name: pinning it would give
        // the family two rows with one name, which the pins file forbids.
        let pinned = regs('a', 'b', 'c', 'd', 'e');
        let remeasured = regs('a', '1', 'c', 'd', 'e');
        let base = pins_with("1.4.1", "8xb300", &pinned);
        let (out, rep) = classify(
            &[feed("1.4.1", "8xb300", &remeasured)],
            &[seen("m", "i", &remeasured)],
            vec![],
            &base,
            None,
        );
        assert_eq!(out, base);
        assert_eq!(rep.name_collisions, vec![row("1.4.1", "8xb300")]);
    }

    #[test]
    fn carried_row_still_published_is_kept_without_new_observation() {
        let r = regs('a', 'b', 'c', 'd', 'e');
        let carry = pins_with("1.4.1", "x", &r);
        let (out, rep) = classify(
            &[feed("1.4.1", "x", &r)],
            &[],
            vec![],
            &empty(),
            Some(&carry),
        );
        assert!(out.find(&r).is_some());
        assert_eq!(rep.added.len(), 1, "carried rows are listed as additions");
        assert!(rep.added[0].1.is_empty(), "not seen today");
    }

    #[test]
    fn carried_row_seen_today_is_added_not_counted_as_already_pinned() {
        let r = regs('a', 'b', 'c', 'd', 'e');
        let carry = pins_with("1.4.1", "x", &r);
        let (_, rep) = classify(
            &[feed("1.4.1", "x", &r)],
            &[seen("m", "i", &r)],
            vec![],
            &empty(),
            Some(&carry),
        );
        assert_eq!(rep.already_pinned, 0, "not pinned on main yet");
        assert_eq!(rep.added.len(), 1);
        assert_eq!(rep.added[0].1[0].instance_id, "i");
    }

    #[test]
    fn carried_row_withdrawn_from_feed_is_dropped() {
        let r = regs('a', 'b', 'c', 'd', 'e');
        let carry = pins_with("1.4.1", "x", &r);
        let (out, rep) = classify(&[], &[], vec![], &empty(), Some(&carry));
        assert!(out.families.is_empty());
        assert_eq!(rep.withdrawn_before_merge, vec![row("1.4.1", "x")]);
    }

    #[test]
    fn base_row_missing_from_feed_is_kept_and_reported() {
        let r = regs('a', 'b', 'c', 'd', 'e');
        let base = pins_with("1.3.1", "gone", &r);
        let (out, rep) = classify(&[], &[], vec![], &base, None);
        assert_eq!(out, base);
        assert_eq!(rep.pinned_not_published, vec![row("1.3.1", "gone")]);
    }

    #[test]
    fn output_is_deterministic_regardless_of_input_order() {
        let r1 = regs('a', '1', 'c', 'd', 'e');
        let r2 = regs('a', '2', 'c', 'd', 'e');
        let f = [feed("1.4.1", "b", &r2), feed("1.4.1", "a", &r1)];
        let (o1, _) = classify(
            &f,
            &[seen("m", "i1", &r1), seen("m", "i2", &r2)],
            vec![],
            &empty(),
            None,
        );
        let (o2, _) = classify(
            &f,
            &[seen("m", "i2", &r2), seen("m", "i1", &r1)],
            vec![],
            &empty(),
            None,
        );
        assert_eq!(o1.to_canonical_json(), o2.to_canonical_json());
    }

    #[test]
    fn feed_registers_are_normalised() {
        let json = format!(
            r#"[{{"version":"1.4.1","name":"x","mrtd":"0x{m}","runtime_rtmrs":{{"RTMR0":"{r}","RTMR1":"{r}","RTMR2":"{r}","RTMR3":"{r}"}},"gpu_count":8}}]"#,
            m = "AB".repeat(48),
            r = "CD".repeat(48)
        );
        let rows = parse_feed(&json).unwrap();
        assert_eq!(rows[0].registers.mrtd, "ab".repeat(48));
        assert_eq!(rows[0].registers.rtmr3, "cd".repeat(48));
    }

    #[test]
    fn feed_rows_with_unsafe_names_are_skipped() {
        // Names and versions reach the PR body and the pins file; only the
        // characters Chutes actually uses are accepted.
        let g = "cd".repeat(48);
        let row = |version: &str, name: &str| {
            serde_json::json!({
                "version": version, "name": name, "mrtd": g,
                "runtime_rtmrs": {"RTMR0": g, "RTMR1": g, "RTMR2": g, "RTMR3": g},
            })
        };
        let json = serde_json::json!([
            row(
                "1.4.1",
                "8xpro_6000 [10.2.1, numa-124c-768g] (58443435b208)"
            ),
            row("1.4.1", "x`@team"),
            row("1.4.1", "line\nbreak"),
            row("1.4.1<", "ok"),
            row("1.4.1", &"y".repeat(81)),
        ])
        .to_string();
        let rows = parse_feed(&json).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].name,
            "8xpro_6000 [10.2.1, numa-124c-768g] (58443435b208)"
        );
    }

    #[test]
    fn feed_rows_with_bad_registers_are_skipped() {
        let g = "cd".repeat(48);
        let json = format!(
            r#"[
            {{"version":"1.4.1","name":"ok","mrtd":"{g}","runtime_rtmrs":{{"RTMR0":"{g}","RTMR1":"{g}","RTMR2":"{g}","RTMR3":"{g}"}}}},
            {{"version":"1.4.1","name":"null","mrtd":"{g}","runtime_rtmrs":null}},
            {{"version":"1.4.1","name":"short","mrtd":"ab","runtime_rtmrs":{{"RTMR0":"{g}","RTMR1":"{g}","RTMR2":"{g}","RTMR3":"{g}"}}}}
        ]"#
        );
        let rows = parse_feed(&json).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["ok"]
        );
    }
}
