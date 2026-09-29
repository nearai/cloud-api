//! Placement decision reporting: the routing-only request view read from
//! `ChatCompletionParams`, and turning a [`DecisionRecord`] into metrics +
//! one log line. None of this touches `Fleet` state — it is pure translation
//! from a placer decision (or the request params) to observability output,
//! extracted out of `fleet.rs` to keep that file to routing/reservation state.

use super::tracing_headers;
use crate::placement_io::{
    PlacementHandles, METRIC_AFFINITY, METRIC_CHOSEN_BACKLOG, METRIC_DECISIONS, METRIC_EXCLUDED,
    METRIC_LANE_CAP, METRIC_LANE_SIZE, METRIC_PLACE_DURATION_US, METRIC_REFUSED,
};
use crate::ChatCompletionParams;
use placement::affinity::AffinityKey;
use placement::decision::{AffinitySource, DecisionRecord};
use placement::policy::{Class, Tier};
use placement::rules::Rule;

/// Routing-only placement inputs: the pool's typed `params.placement`, the
/// operator priority and the tracing ids (read from `params.extra` before
/// the tracing helper strips them). Holds an [`AffinityKey`], so it has no
/// `Debug`; the key is never logged.
pub(super) struct PlacementRequest {
    pub(super) model: String,
    /// `model:{model}`, the decision and latency metrics' model tag. Model
    /// names come from the catalog, so the tag stays low-cardinality.
    pub(super) model_tag: String,
    /// Tracing ids for the decision log line (empty when absent).
    pub(super) request_id: String,
    pub(super) org_id: String,
    /// The pool's estimate (input only; 0 when the pool gave none).
    pub(super) prompt_tokens: u64,
    /// The pool's context requirement (input plus output reserve).
    pub(super) context_tokens: Option<u64>,
    /// The pool's class: the requirement exceeds the base tier.
    pub(super) heavy: bool,
    /// The lane class: the prompt alone exceeds the base tier.
    pub(super) prefill_heavy: bool,
    pub(super) affinity: Option<AffinityKey>,
    pub(super) affinity_source: AffinitySource,
    /// `params.request_priority` (operator-set, never client JSON).
    pub(super) priority: i32,
    /// The `size:` latency tag: the pool's prompt estimate, bucketed.
    pub(super) size: &'static str,
}

impl PlacementRequest {
    /// Reads `params.placement`, `params.request_priority` and the tracing
    /// ids without removing anything. Placement never re-estimates: the pool
    /// (`inference_provider_pool::context_routing`) owns the size estimate.
    pub(super) fn from_params(params: &ChatCompletionParams) -> Self {
        let text = |key: &str| params.extra.get(key).and_then(|value| value.as_str());
        let placement = &params.placement;
        Self {
            model: params.model.clone(),
            model_tag: format!("model:{}", params.model),
            request_id: text(tracing_headers::REQUEST_ID)
                .unwrap_or_default()
                .to_string(),
            org_id: text(tracing_headers::ORG_ID)
                .unwrap_or_default()
                .to_string(),
            prompt_tokens: placement.prompt_tokens.unwrap_or(0),
            context_tokens: placement.context_tokens,
            heavy: placement.heavy,
            prefill_heavy: placement.prefill_heavy,
            affinity: placement.affinity.clone(),
            // A source without a key would mislabel the decision record.
            affinity_source: if placement.affinity.is_some() {
                placement.affinity_source
            } else {
                AffinitySource::None
            },
            priority: params.request_priority,
            size: size_tag(placement.prompt_tokens),
        }
    }
}

/// Decision metrics and one log line per decision. IDs and numbers only:
/// never the affinity key, pin id or content. Metric tags stay
/// low-cardinality: static strings plus the request's catalog model tag.
pub(super) fn report_decision(
    handles: &PlacementHandles,
    record: &DecisionRecord,
    request: &PlacementRequest,
) {
    let metrics = handles.io.metrics();
    let outcome = outcome_tag(record.outcome);
    let tier = tier_tag(record.tier);
    let class = class_tag(record.class);
    let band = band_tag(record.priority_band);
    metrics.record_count(
        METRIC_DECISIONS,
        1,
        &[
            outcome,
            tier,
            class,
            strategy_tag(record.strategy),
            band,
            detail_tag(record),
            &request.model_tag,
        ],
    );
    if record.outcome == "refused" {
        metrics.record_count(METRIC_REFUSED, 1, &[tier, class, band, &request.model_tag]);
    }
    metrics.record_histogram(METRIC_LANE_SIZE, f64::from(record.lane_size), &[tier]);
    metrics.record_histogram(METRIC_LANE_CAP, f64::from(record.lane_cap), &[tier]);
    metrics.record_histogram(
        METRIC_PLACE_DURATION_US,
        f64::from(record.place_us),
        &[tier],
    );
    metrics.record_count(
        METRIC_AFFINITY,
        1,
        &[affinity_tag(record.affinity), outcome],
    );
    for (rule, n) in record.excluded.iter().filter(|(_, n)| *n > 0) {
        metrics.record_count(METRIC_EXCLUDED, i64::from(*n), &[rule_tag(*rule)]);
    }
    if let Some(backlog) = record.chosen_backlog_tokens {
        metrics.record_histogram(METRIC_CHOSEN_BACKLOG, backlog as f64, &[]);
    }
    let excluded = record
        .excluded
        .iter()
        .map(|(rule, n)| format!("{}:{n}", rule.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    // One field set for both levels. Placement with no usable state
    // (Valkey unreachable, endpoint misconfigured, hosts that publish no
    // frames, stale snapshot, kill switch) logs at debug: every such
    // request would otherwise repeat the same line. The decision metric above still counts each one.
    macro_rules! decision_line {
        ($level:ident) => {
            tracing::$level!(
                request_id = %request.request_id,
                org_id = %request.org_id,
                model = %request.model,
                tier = record.tier.as_str(),
                class = record.class.as_str(),
                priority_band = record.priority_band,
                prompt_tokens = record.prompt_tokens,
                context_tokens = ?record.context_tokens,
                outcome = record.outcome,
                strategy = record.strategy.unwrap_or(""),
                selection = record.selection.unwrap_or(""),
                reason = record.reason.unwrap_or(""),
                slot = record.slot.as_deref().unwrap_or(""),
                home = record.home.as_deref().unwrap_or(""),
                pinned = record.pinned.as_deref().unwrap_or(""),
                eligible = record.eligible,
                excluded = %excluded,
                lane_size = record.lane_size,
                lane_cap = record.lane_cap,
                chosen_score = ?record.chosen_score,
                home_score = ?record.home_score,
                best_score = ?record.best_score,
                pending_tok = record.pending_tok,
                chosen_backlog_tokens = ?record.chosen_backlog_tokens,
                snapshot_age_ms = record.snapshot_age_ms,
                place_us = record.place_us,
                "Placement decision"
            )
        };
    }
    if logs_at_debug(record) {
        decision_line!(debug);
    } else {
        decision_line!(info);
    }
}

/// `strategy:`/`selection:`/`size:` tags for the provider's latency
/// histograms: the placed decision's labels, or `legacy` for a request
/// placement did not place, and the request's size bucket ([`size_tag`]).
pub(super) fn latency_tags(
    record: Option<&DecisionRecord>,
    size: &'static str,
) -> [&'static str; 3] {
    match record {
        Some(record) => [
            strategy_tag(record.strategy),
            match record.selection {
                Some(selection) => selection_tag(selection),
                None => "selection:none",
            },
            size,
        ],
        None => ["strategy:legacy", "selection:legacy", size],
    }
}

/// `size:` bucket of the pool's prompt-token estimate (decimal thousands;
/// the last boundary is the long tier's 100K), `unknown` when the pool gave
/// none.
pub(super) fn size_tag(prompt_tokens: Option<u64>) -> &'static str {
    match prompt_tokens {
        None => "size:unknown",
        Some(0..=8_000) => "size:le8k",
        Some(8_001..=32_000) => "size:le32k",
        Some(32_001..=100_000) => "size:le100k",
        Some(_) => "size:gt100k",
    }
}

/// Legacy decisions made because placement has no usable snapshot, is
/// switched off by the data-plane kill switch, or had its refusal turned
/// into legacy because refusals are off (the default), are logged at debug;
/// they would otherwise repeat on every such request while the shared state
/// is down, unconfigured, a switch is on or refusals are off. The decision
/// metric still counts each.
fn logs_at_debug(record: &DecisionRecord) -> bool {
    record.outcome == "legacy"
        && matches!(
            record.reason,
            Some("no_state" | "stale" | "disabled" | "refuse_off")
        )
}

fn outcome_tag(outcome: &str) -> &'static str {
    match outcome {
        "place" => "outcome:place",
        "legacy" => "outcome:legacy",
        "refused" => "outcome:refused",
        _ => "outcome:unknown",
    }
}

fn selection_tag(selection: &str) -> &'static str {
    match selection {
        "pinned" => "selection:pinned",
        "home" => "selection:home",
        "spill" => "selection:spill",
        "best_of_two" => "selection:best_of_two",
        _ => "selection:unknown",
    }
}

fn tier_tag(tier: Tier) -> &'static str {
    match tier {
        Tier::Base => "tier:base",
        Tier::Long => "tier:long",
    }
}

fn class_tag(class: Class) -> &'static str {
    match class {
        Class::Short => "class:short",
        Class::Heavy => "class:heavy",
    }
}

fn band_tag(band: &str) -> &'static str {
    match band {
        "neg" => "priority_band:neg",
        "normal" => "priority_band:normal",
        "high" => "priority_band:high",
        _ => "priority_band:unknown",
    }
}

/// A legacy decision has no strategy (`none`).
fn strategy_tag(strategy: Option<&str>) -> &'static str {
    match strategy {
        None => "strategy:none",
        Some("short_clean") => "strategy:short_clean",
        Some("short_overflow") => "strategy:short_overflow",
        Some("heavy_long") => "strategy:heavy_long",
        Some("heavy_lane_join") => "strategy:heavy_lane_join",
        Some("heavy_lane_admit") => "strategy:heavy_lane_admit",
        Some("refuse") => "strategy:refuse",
        Some(_) => "strategy:unknown",
    }
}

/// `reason:{..}` for a legacy or refused record, `selection:{..}` for a
/// placed one.
fn detail_tag(record: &DecisionRecord) -> &'static str {
    match (record.reason, record.selection) {
        (Some(reason), _) => match reason {
            "disabled" => "reason:disabled",
            "no_state" => "reason:no_state",
            "stale" => "reason:stale",
            "none_eligible" => "reason:none_eligible",
            "host_unmapped" => "reason:host_unmapped",
            "key_group" => "reason:key_group",
            "incomplete" => "reason:incomplete",
            "lane_full" => "reason:lane_full",
            "long_full" => "reason:long_full",
            "refuse_off" => "reason:refuse_off",
            _ => "reason:unknown",
        },
        (None, Some(selection)) => selection_tag(selection),
        (None, None) => "reason:unknown",
    }
}

fn affinity_tag(affinity: &str) -> &'static str {
    match affinity {
        "client" => "affinity:client",
        "prefix" => "affinity:prefix",
        "none" => "affinity:none",
        _ => "affinity:unknown",
    }
}

fn rule_tag(rule: Rule) -> &'static str {
    match rule {
        Rule::Lifecycle => "rule:lifecycle",
        Rule::Freshness => "rule:freshness",
        Rule::Capacity => "rule:capacity",
        Rule::Context => "rule:context",
        Rule::Lane => "rule:lane",
    }
}

#[cfg(test)]
mod tests {
    use super::logs_at_debug;
    use placement::decision::{AffinitySource, Decision, DecisionRecord, PlaceInput, Placer};
    use placement::policy::Tier;
    use placement::snapshot::Snapshot;
    use std::collections::HashMap;

    fn legacy_record(snap: &Snapshot) -> DecisionRecord {
        let input = PlaceInput {
            model: "z-ai/glm-5.3-flash".to_string(),
            prompt_tokens: 10,
            context_tokens: None,
            heavy: false,
            prefill_heavy: false,
            priority: 0,
            affinity: None,
            affinity_source: AffinitySource::None,
            now_ms: 10_000,
        };
        let mut rng = rand::rng();
        match Placer::new([1u8; 32], Tier::Base).place(&input, snap, &HashMap::new(), &mut rng) {
            Decision::Legacy { record, .. } => record,
            Decision::Place { .. } | Decision::Refused { .. } => {
                panic!("an empty snapshot is always legacy")
            }
        }
    }

    fn no_state_record() -> DecisionRecord {
        legacy_record(&Snapshot::default())
    }

    #[test]
    fn no_state_and_stale_decisions_log_at_debug() {
        let mut record = no_state_record();
        assert_eq!(record.reason, Some("no_state"));
        assert!(logs_at_debug(&record));
        record.reason = Some("stale");
        assert!(logs_at_debug(&record));
    }

    #[test]
    fn kill_switch_decisions_log_at_debug_with_their_own_tag() {
        let record = legacy_record(&Snapshot {
            disabled: true,
            ..Snapshot::default()
        });
        assert_eq!(record.reason, Some("disabled"));
        assert!(logs_at_debug(&record));
        assert_eq!(super::detail_tag(&record), "reason:disabled");
    }

    #[test]
    fn other_decisions_log_at_info() {
        let mut record = no_state_record();
        record.reason = Some("incomplete");
        assert!(!logs_at_debug(&record));
        record.outcome = "place";
        record.reason = None;
        assert!(!logs_at_debug(&record));
    }
}

#[cfg(test)]
mod observability_tests {
    use super::*;
    use crate::placement_io::{
        PlacementIo, PlacementMetrics, METRIC_DECISIONS, METRIC_LANE_CAP, METRIC_LANE_SIZE,
        METRIC_PLACE_DURATION_US, METRIC_REFUSED,
    };
    use arc_swap::ArcSwap;
    use placement::affinity::pin_id;
    use placement::decision::{AffinitySource, Decision, LegacyReason, PlaceInput, Placer};
    use placement::policy::RoutePolicy;
    use placement::snapshot::Snapshot;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type Recorded = Vec<(String, f64, Vec<String>)>;

    #[derive(Default)]
    struct FakeMetrics {
        counts: Mutex<Recorded>,
        histograms: Mutex<Recorded>,
    }

    impl PlacementMetrics for FakeMetrics {
        fn record_count(&self, name: &str, value: i64, tags: &[&str]) {
            self.counts.lock().unwrap().push((
                name.to_string(),
                value as f64,
                tags.iter().map(|t| t.to_string()).collect(),
            ));
        }
        fn record_histogram(&self, name: &str, value: f64, tags: &[&str]) {
            self.histograms.lock().unwrap().push((
                name.to_string(),
                value,
                tags.iter().map(|t| t.to_string()).collect(),
            ));
        }
    }

    fn handles(metrics: Arc<FakeMetrics>) -> PlacementHandles {
        let (io, _writes) = PlacementIo::for_test(metrics);
        PlacementHandles {
            placer: Arc::new(Placer::new([1u8; 32], Tier::Base)),
            io,
            hosts: Arc::new(ArcSwap::from_pointee(crate::BackendHosts::default())),
        }
    }

    fn request(affinity: Option<AffinityKey>) -> PlacementRequest {
        PlacementRequest {
            model: "z-ai/glm-5.3-flash".to_string(),
            model_tag: "model:z-ai/glm-5.3-flash".to_string(),
            request_id: "req-1".to_string(),
            org_id: "org-1".to_string(),
            prompt_tokens: 10,
            context_tokens: Some(20),
            heavy: false,
            prefill_heavy: false,
            affinity_source: if affinity.is_some() {
                AffinitySource::Client
            } else {
                AffinitySource::None
            },
            affinity,
            priority: 0,
            size: "size:le8k",
        }
    }

    fn legacy(reason: &'static str) -> DecisionRecord {
        let input = PlaceInput {
            model: "z-ai/glm-5.3-flash".to_string(),
            prompt_tokens: 10,
            context_tokens: None,
            heavy: false,
            prefill_heavy: false,
            priority: 0,
            affinity: None,
            affinity_source: AffinitySource::None,
            now_ms: 10_000,
        };
        let mut rng = rand::rng();
        let mut record = match Placer::new([1u8; 32], Tier::Base).place(
            &input,
            &Snapshot::default(),
            &HashMap::new(),
            &mut rng,
        ) {
            Decision::Legacy { record, .. } => record,
            _ => panic!("an empty snapshot is always legacy"),
        };
        record.reason = Some(reason);
        record
    }

    fn refused(tier: Tier, reason: &'static str) -> DecisionRecord {
        let mut record = legacy("no_state");
        record.outcome = "refused";
        record.reason = Some(reason);
        record.tier = tier;
        record.class = Class::Heavy;
        record.priority_band = "neg";
        record.strategy = Some("refuse");
        record.lane_size = 4;
        record.lane_cap = 2;
        record.place_us = 37;
        record
    }

    fn placed(strategy: &'static str, selection: &'static str) -> DecisionRecord {
        let mut record = legacy("no_state");
        record.outcome = "place";
        record.reason = None;
        record.strategy = Some(strategy);
        record.selection = Some(selection);
        record
    }

    fn tags_of(recorded: &Recorded, name: &str) -> Vec<Vec<String>> {
        recorded
            .iter()
            .filter(|(n, _, _)| n == name)
            .map(|(_, _, t)| t.clone())
            .collect()
    }

    /// Every value the placer can emit maps to its own static tag, never to
    /// an `unknown` fallback.
    #[test]
    fn every_outcome_reason_selection_strategy_is_static_tag() {
        for outcome in ["place", "legacy", "refused"] {
            assert!(!outcome_tag(outcome).ends_with("unknown"), "{outcome}");
        }
        assert!(outcome_tag("bogus").ends_with("unknown"));

        let legacy_reasons = [
            LegacyReason::Disabled,
            LegacyReason::NoState,
            LegacyReason::Stale,
            LegacyReason::NoneEligible,
            LegacyReason::HostUnmapped,
            LegacyReason::KeyGroup,
            LegacyReason::Incomplete,
        ]
        .map(LegacyReason::as_str);
        for reason in legacy_reasons
            .into_iter()
            .chain(["lane_full", "long_full", "refuse_off"])
        {
            let tag = detail_tag(&legacy(reason));
            assert_eq!(tag, format!("reason:{reason}"), "{reason}");
        }

        for selection in ["pinned", "home", "spill", "best_of_two"] {
            assert_eq!(
                detail_tag(&placed("short_clean", selection)),
                format!("selection:{selection}")
            );
        }

        for strategy in [
            RoutePolicy::ShortClean,
            RoutePolicy::ShortOverflow,
            RoutePolicy::HeavyLong,
            RoutePolicy::HeavyLaneJoin,
            RoutePolicy::HeavyLaneAdmit,
            RoutePolicy::Refuse,
        ] {
            assert_eq!(
                strategy_tag(Some(strategy.as_str())),
                format!("strategy:{}", strategy.as_str())
            );
        }
        assert_eq!(strategy_tag(None), "strategy:none");

        for tier in [Tier::Base, Tier::Long] {
            assert_eq!(tier_tag(tier), format!("tier:{}", tier.as_str()));
        }
        for class in [Class::Short, Class::Heavy] {
            assert_eq!(class_tag(class), format!("class:{}", class.as_str()));
        }
        for band in ["neg", "normal", "high"] {
            assert_eq!(band_tag(band), format!("priority_band:{band}"));
        }
        assert_eq!(
            latency_tags(Some(&placed("heavy_long", "home")), "size:gt100k"),
            ["strategy:heavy_long", "selection:home", "size:gt100k"]
        );
        assert_eq!(
            latency_tags(None, "size:unknown"),
            ["strategy:legacy", "selection:legacy", "size:unknown"]
        );
    }

    #[test]
    fn size_buckets_are_static_tags() {
        for (tokens, tag) in [
            (None, "size:unknown"),
            (Some(0), "size:le8k"),
            (Some(8_000), "size:le8k"),
            (Some(8_001), "size:le32k"),
            (Some(32_000), "size:le32k"),
            (Some(32_001), "size:le100k"),
            (Some(100_000), "size:le100k"),
            (Some(100_001), "size:gt100k"),
            (Some(u64::MAX), "size:gt100k"),
        ] {
            assert_eq!(size_tag(tokens), tag, "{tokens:?}");
        }
    }

    #[test]
    fn refused_metric_tags_exact() {
        for (tier, reason) in [(Tier::Base, "lane_full"), (Tier::Long, "long_full")] {
            let metrics = Arc::new(FakeMetrics::default());
            report_decision(
                &handles(metrics.clone()),
                &refused(tier, reason),
                &request(None),
            );

            let tier_tag = format!("tier:{}", tier.as_str());
            let counts = metrics.counts.lock().unwrap();
            assert_eq!(
                tags_of(&counts, METRIC_REFUSED),
                vec![vec![
                    tier_tag.clone(),
                    "class:heavy".to_string(),
                    "priority_band:neg".to_string(),
                    "model:z-ai/glm-5.3-flash".to_string(),
                ]]
            );
            assert_eq!(
                tags_of(&counts, METRIC_DECISIONS),
                vec![vec![
                    "outcome:refused".to_string(),
                    tier_tag.clone(),
                    "class:heavy".to_string(),
                    "strategy:refuse".to_string(),
                    "priority_band:neg".to_string(),
                    format!("reason:{reason}"),
                    "model:z-ai/glm-5.3-flash".to_string(),
                ]]
            );
            let histograms = metrics.histograms.lock().unwrap();
            for (name, value) in [
                (METRIC_LANE_SIZE, 4.0),
                (METRIC_LANE_CAP, 2.0),
                (METRIC_PLACE_DURATION_US, 37.0),
            ] {
                let found: Vec<_> = histograms.iter().filter(|(n, _, _)| n == name).collect();
                assert_eq!(found.len(), 1, "{name}");
                assert_eq!(found[0].1, value, "{name}");
                assert_eq!(found[0].2, vec![tier_tag.clone()], "{name}");
            }
        }
    }

    #[test]
    fn non_refused_decisions_do_not_count_refusals() {
        let metrics = Arc::new(FakeMetrics::default());
        let h = handles(metrics.clone());
        report_decision(&h, &legacy("no_state"), &request(None));
        report_decision(&h, &placed("short_clean", "home"), &request(None));
        assert!(tags_of(&metrics.counts.lock().unwrap(), METRIC_REFUSED).is_empty());
    }

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs `f` with a debug-level tracing capture and returns what it wrote.
    fn captured(f: impl FnOnce()) -> String {
        let buf = Capture(Arc::new(Mutex::new(Vec::new())));
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn decision_log_has_no_key_material() {
        let secret = [0x5Au8; 32];
        let key = AffinityKey::from_bytes([0xAB; 16]);
        let pin_hex = pin_id(Tier::Base, &key, &secret).to_hex();
        let mut record = placed("short_clean", "pinned");
        record.slot = Some("host-a#1".to_string());
        record.home = Some("host-a#0".to_string());
        record.pinned = Some("host-a#1".to_string());
        record.affinity = "client";

        let metrics = Arc::new(FakeMetrics::default());
        let h = handles(metrics.clone());
        let out = captured(|| report_decision(&h, &record, &request(Some(key))));

        assert!(out.contains("Placement decision"), "{out}");
        for field in [
            "request_id=req-1",
            "org_id=org-1",
            "tier=\"base\"",
            "class=\"short\"",
            "priority_band=\"normal\"",
            "prompt_tokens=10",
            "outcome=\"place\"",
            "strategy=\"short_clean\"",
            "selection=\"pinned\"",
            "slot=\"host-a#1\"",
            "home=\"host-a#0\"",
            "pinned=\"host-a#1\"",
            "lane_size=",
            "lane_cap=",
            "pending_tok=",
            "chosen_backlog_tokens=",
            "snapshot_age_ms=",
            "place_us=",
        ] {
            assert!(out.contains(field), "missing {field}: {out}");
        }
        // Neither the key bytes, the pin id nor the pin secret, in any of
        // the encodings a formatter could produce.
        let lower = out.to_lowercase();
        for forbidden in [
            "abab".to_string(),
            "171, 171".to_string(),
            "0xab".to_string(),
            pin_hex,
            "5a5a".to_string(),
            "affinitykey".to_string(),
        ] {
            assert!(!lower.contains(&forbidden), "leaked {forbidden}: {out}");
        }
    }

    #[test]
    fn decision_log_levels() {
        let h = handles(Arc::new(FakeMetrics::default()));
        for reason in ["no_state", "stale", "disabled", "refuse_off"] {
            let out = captured(|| report_decision(&h, &legacy(reason), &request(None)));
            assert!(out.contains("DEBUG"), "{reason}: {out}");
            assert!(!out.contains("INFO"), "{reason}: {out}");
        }
        for record in [
            legacy("incomplete"),
            placed("short_clean", "home"),
            refused(Tier::Long, "long_full"),
        ] {
            let out = captured(|| report_decision(&h, &record, &request(None)));
            assert!(out.contains("INFO"), "{out}");
        }
    }
}
