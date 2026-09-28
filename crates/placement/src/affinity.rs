//! Warm-cache affinity: a bounded HRW ranked walk plus a follow pin.
//!
//! [`select`] is the crate's entry point: given an optional affinity key,
//! an optional follow pin, and the current eligible-host scores, it decides
//! which host to route to and whether a new pin should be written. There is
//! no I/O here — `PinTable` is an in-memory table the caller (Task 6) loads
//! from and persists to Valkey off the request path.
//!
//! Privacy: [`AffinityKey`] and [`PinId`] are derived from customer content
//! (the conversation/session identity), so neither implements `Debug`,
//! `Display`, nor `Serialize`. Never log them.

use std::collections::HashMap;

use hmac::{Hmac, KeyInit, Mac};
use rand::{Rng, RngExt};
use sha2::{Digest, Sha256};

use crate::consts::{AFFINITY_ABS_SLACK, AFFINITY_EPS, PIN_TTL_MS};

/// An opaque, unlinkable-by-inspection affinity key (e.g. derived from a
/// conversation id). Intentionally has no `Debug`/`Display`/`Serialize`.
#[derive(Clone)]
pub struct AffinityKey([u8; 16]);

impl AffinityKey {
    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }

    /// Lowercase hex encoding of the 16 key bytes, for the caller to carry
    /// the key through an opaque channel (e.g. `params.extra`) between the
    /// request path that derives it and the placement decision that reads
    /// it. This is an explicit, deliberate conversion — not a `Display`/
    /// `Debug`/`Serialize` impl — so it never fires from a `{:?}`/log call;
    /// callers must still never log the returned string.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Inverse of [`Self::to_hex`]: parses a lowercase (or uppercase) hex
    /// string back into an `AffinityKey`. Returns `None` if `s` is not
    /// exactly 32 hex characters.
    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = hex::decode(s).ok()?;
        let arr: [u8; 16] = bytes.try_into().ok()?;
        Some(Self(arr))
    }
}

/// An opaque follow-pin identifier: `HMAC-SHA256(pin_secret, key)`,
/// truncated to 16 bytes. Written to Valkey as hex via [`PinId::to_hex`].
/// Intentionally has no `Debug`/`Display`/`Serialize`.
pub struct PinId([u8; 16]);

impl PinId {
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

type HmacSha256 = Hmac<Sha256>;

/// Derive a [`PinId`] for `key`, keyed by the deployment's `pin_secret`.
pub fn pin_id(key: &AffinityKey, pin_secret: &[u8; 32]) -> PinId {
    // A 32-byte key is always valid for HMAC-SHA256; this never fails.
    let mut mac = HmacSha256::new_from_slice(pin_secret).expect("32-byte HMAC key is always valid");
    mac.update(&key.0);
    let digest = mac.finalize().into_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    PinId(out)
}

/// In-memory follow-pin table: pin id -> (host, written-at ms). The caller
/// loads this from Valkey and persists writes back; this crate never talks
/// to Valkey directly.
///
/// `Clone` so a reader can copy-on-write it behind an `Arc` (see
/// `Snapshot::pins`).
#[derive(Clone, Default)]
pub struct PinTable {
    entries: HashMap<[u8; 16], (String, u64)>,
}

impl PinTable {
    /// The pinned `(host, written-at ms)`, if `id` has an entry that hasn't
    /// expired: valid while `now_ms < at_ms + PIN_TTL_MS` (saturating, so an
    /// `at_ms` from the far past or a clock skew never panics or wraps to
    /// "still valid").
    pub fn get(&self, id: &PinId, now_ms: u64) -> Option<(&str, u64)> {
        let (host, at_ms) = self.entries.get(&id.0)?;
        if now_ms < at_ms.saturating_add(PIN_TTL_MS) {
            Some((host.as_str(), *at_ms))
        } else {
            None
        }
    }

    /// Write (or overwrite) the pin for `id`.
    pub fn insert(&mut self, id: [u8; 16], host: String, at_ms: u64) {
        self.entries.insert(id, (host, at_ms));
    }

    /// Drop every entry [`Self::get`] would already treat as expired at
    /// `now_ms`, so a long-lived table stays bounded.
    pub fn prune(&mut self, now_ms: u64) {
        self.entries
            .retain(|_, (_, at_ms)| now_ms < at_ms.saturating_add(PIN_TTL_MS));
    }

    /// Number of entries, expired or not.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Rendezvous-hash (HRW) rank of `hosts` for `key`: descending by
/// `sha256(key ‖ host)[..8]` as a big-endian `u64`. Deterministic for a
/// given `(key, hosts)` pair, and stable under removal — dropping a host
/// from the input never reorders the others (classic HRW property, since
/// each host's score depends only on `key` and itself).
pub fn hrw_rank(key: &AffinityKey, hosts: &[&str]) -> Vec<String> {
    let mut scored: Vec<(u64, &str)> = hosts
        .iter()
        .map(|h| {
            let mut hasher = Sha256::new();
            hasher.update(key.0);
            hasher.update(h.as_bytes());
            let digest = hasher.finalize();
            let val = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
            (val, *h)
        })
        .collect();
    // Break ties (astronomically unlikely for real sha256 output, but keeps
    // the ordering total and deterministic) by host name.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, h)| h.to_string()).collect()
}

/// A selection outcome. `rank` in `Spill` is 1-based position in the HRW
/// walk (`Home` is implicitly rank 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The caller's follow pin was honored.
    Pinned,
    /// The key's top-ranked (HRW rank 1) host was chosen.
    Home,
    /// A lower-ranked host was chosen because Home (and any host ranked
    /// before it) was over the affinity bound.
    Spill { rank: u8 },
    /// No affinity key: chose the lower-scoring of two uniformly sampled
    /// hosts.
    BestOfTwo,
}

/// The result of [`select`].
#[derive(Clone, Debug, PartialEq)]
pub struct Selected {
    pub host: String,
    pub selection: Selection,
    /// The key's HRW rank-1 host, for keyed selections (`Pinned`, `Home`,
    /// `Spill`). `None` for keyless `BestOfTwo`.
    pub home: Option<String>,
    /// Whether the caller should (re)write the follow pin: true when the
    /// chosen host differs from home (a spill), or when an existing pin
    /// moved elsewhere (including being dropped back to home).
    pub write_pin: bool,
}

/// The bounded-load affinity test: is `score` close enough to the fleet's
/// `best` score to keep routing there?
///
/// `bound = max(best * (1 + EPS), best + ABS_SLACK)` — the absolute slack
/// keeps affinity intact when the whole fleet is near idle and `best` is
/// close to 0, where a purely relative bound would collapse to ~0 and
/// reject any nonzero score.
fn within_bound(score: f64, best: f64) -> bool {
    let bound = f64::max(best * (1.0 + AFFINITY_EPS), best + AFFINITY_ABS_SLACK);
    score <= bound
}

/// Decide which host to route to. `scores` holds eligible hosts only (the
/// caller has already applied `rules.rs`); returns `None` when `scores` is
/// empty, so the caller falls back to the legacy `Fleet::acquire_index`
/// path. See the module doc and `Selection` for the decision rules.
pub fn select(
    key: Option<&AffinityKey>,
    pin: Option<&str>,
    scores: &[(String, f64)],
    rng: &mut impl Rng,
) -> Option<Selected> {
    if scores.is_empty() {
        return None;
    }
    let best = scores.iter().map(|(_, s)| *s).fold(f64::INFINITY, f64::min);
    let score_of = |host: &str| scores.iter().find(|(h, _)| h == host).map(|(_, s)| *s);

    // The HRW rank, only computed when a key is present. Keyed selections
    // (Pinned/Home/Spill) always report `home = Some(rank[0])`; keyless
    // BestOfTwo reports `None`.
    let rank: Option<Vec<String>> = key.map(|k| {
        let hosts: Vec<&str> = scores.iter().map(|(h, _)| h.as_str()).collect();
        hrw_rank(k, &hosts)
    });
    let home = rank.as_ref().map(|r| r[0].clone());

    // Rule 1: an in-bound pin wins outright.
    if let Some(pin_host) = pin {
        if let Some(score) = score_of(pin_host) {
            if within_bound(score, best) {
                return Some(Selected {
                    host: pin_host.to_string(),
                    selection: Selection::Pinned,
                    home: home.clone(),
                    write_pin: false,
                });
            }
        }
    }

    // Rule 2: keyed HRW walk — first host in rank within bound.
    if let Some(rank) = rank {
        let chosen_idx = rank
            .iter()
            .position(|h| score_of(h).is_some_and(|s| within_bound(s, best)))
            .unwrap_or_else(|| {
                // Unreachable in practice: the best-scoring host is always
                // within its own bound. Kept as a no-panic fallback to the
                // min-score host, per the crate's fail-open posture.
                rank.iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        let sa = score_of(a).unwrap_or(f64::INFINITY);
                        let sb = score_of(b).unwrap_or(f64::INFINITY);
                        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        let chosen_host = rank[chosen_idx].clone();
        let selection = if chosen_idx == 0 {
            Selection::Home
        } else {
            Selection::Spill {
                rank: u8::try_from(chosen_idx + 1).unwrap_or(u8::MAX),
            }
        };
        let write_pin = chosen_host != rank[0] || pin.is_some();
        return Some(Selected {
            host: chosen_host,
            selection,
            home: Some(rank[0].clone()),
            write_pin,
        });
    }

    // Rule 3: no key — sample 2 distinct indices uniformly and take the
    // lower score (first-sampled wins ties). A single host is taken as-is.
    if scores.len() == 1 {
        return Some(Selected {
            host: scores[0].0.clone(),
            selection: Selection::BestOfTwo,
            home: None,
            write_pin: false,
        });
    }
    let n = scores.len();
    let i = rng.random_range(0..n);
    let mut j = rng.random_range(0..n);
    while j == i {
        j = rng.random_range(0..n);
    }
    let chosen = if scores[i].1 <= scores[j].1 { i } else { j };
    Some(Selected {
        host: scores[chosen].0.clone(),
        selection: Selection::BestOfTwo,
        home: None,
        write_pin: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::HashMap;

    fn find_key_with_rank(hosts: &[&str], want: &[&str]) -> AffinityKey {
        for seed in 0u128.. {
            let bytes = seed.to_be_bytes();
            let key = AffinityKey::from_bytes(bytes);
            let rank = hrw_rank(&key, hosts);
            if rank.len() >= want.len()
                && rank
                    .iter()
                    .take(want.len())
                    .map(String::as_str)
                    .eq(want.iter().copied())
            {
                return key;
            }
        }
        unreachable!("no key found within u128 search space")
    }

    #[test]
    fn empty_scores_is_none() {
        let mut rng = StdRng::seed_from_u64(1);
        let scores: Vec<(String, f64)> = vec![];
        assert!(select(None, None, &scores, &mut rng).is_none());
    }

    #[test]
    fn home_when_within_bound() {
        let key = find_key_with_rank(&["gpu02", "gpu08"], &["gpu02"]);
        let scores = vec![("gpu02".to_string(), 0.30), ("gpu08".to_string(), 0.28)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(Some(&key), None, &scores, &mut rng).unwrap();
        assert_eq!(sel.host, "gpu02");
        assert_eq!(sel.selection, Selection::Home);
        assert!(!sel.write_pin);
        assert_eq!(sel.home.as_deref(), Some("gpu02"));
    }

    #[test]
    fn idle_fleet_keeps_home() {
        // best (gpu-a) is 0.0; home (gpu-b) is 0.05. Bound = max(0*1.25,
        // 0+0.1) = 0.1, so the absolute slack keeps affinity even though the
        // fleet is near idle and a relative-only bound would collapse to ~0.
        let key = find_key_with_rank(&["gpu-a", "gpu-b"], &["gpu-b"]);
        let scores = vec![("gpu-a".to_string(), 0.0), ("gpu-b".to_string(), 0.05)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(Some(&key), None, &scores, &mut rng).unwrap();
        assert_eq!(sel.host, "gpu-b");
        assert_eq!(sel.selection, Selection::Home);
    }

    #[test]
    fn spill_then_follow_pin() {
        let key = find_key_with_rank(&["gpu02", "gpu08"], &["gpu02", "gpu08"]);
        let mut rng = StdRng::seed_from_u64(1);

        // Turn 1: home (gpu02) is overloaded at 2.0; gpu08 at 0.4 is the
        // next host in rank and within bound (best 0.4, bound 0.5).
        let scores = vec![("gpu02".to_string(), 2.0), ("gpu08".to_string(), 0.4)];
        let sel = select(Some(&key), None, &scores, &mut rng).unwrap();
        assert_eq!(sel.host, "gpu08");
        assert_eq!(sel.selection, Selection::Spill { rank: 2 });
        assert!(sel.write_pin, "spill away from home must write a pin");
        assert_eq!(sel.home.as_deref(), Some("gpu02"));

        // Turn 2: caller now passes the written pin; gpu02 is still hot.
        let scores = vec![("gpu02".to_string(), 2.0), ("gpu08".to_string(), 0.4)];
        let sel = select(Some(&key), Some("gpu08"), &scores, &mut rng).unwrap();
        assert_eq!(sel.host, "gpu08");
        assert_eq!(sel.selection, Selection::Pinned);
        assert!(!sel.write_pin);

        // Turn 3: gpu02 cools to 0.35, gpu08 drifts to 0.42. best = 0.35,
        // bound = max(0.35*1.25=0.4375, 0.35+0.1=0.45) = 0.45; 0.42 <= 0.45
        // so the pin holds (no flap).
        let scores = vec![("gpu02".to_string(), 0.35), ("gpu08".to_string(), 0.42)];
        let sel = select(Some(&key), Some("gpu08"), &scores, &mut rng).unwrap();
        assert_eq!(sel.host, "gpu08");
        assert_eq!(sel.selection, Selection::Pinned);
        assert!(!sel.write_pin);

        // Turn 4: gpu08 spikes to 0.9, past the bound (0.45) — pin is
        // ignored and the walk returns to home (gpu02), moving the pin.
        let scores = vec![("gpu02".to_string(), 0.35), ("gpu08".to_string(), 0.9)];
        let sel = select(Some(&key), Some("gpu08"), &scores, &mut rng).unwrap();
        assert_eq!(sel.host, "gpu02");
        assert_eq!(sel.selection, Selection::Home);
        assert!(sel.write_pin, "pin moving back to home must write a pin");
    }

    #[test]
    fn pin_to_ineligible_host_is_ignored() {
        // pin names a host that isn't in `scores` at all (e.g. it fell out
        // of eligibility); the keyed walk decides instead.
        let key = find_key_with_rank(&["gpu02", "gpu08"], &["gpu02"]);
        let scores = vec![("gpu02".to_string(), 0.2), ("gpu08".to_string(), 0.9)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(Some(&key), Some("gpu05"), &scores, &mut rng).unwrap();
        assert_eq!(sel.host, "gpu02");
        assert_eq!(sel.selection, Selection::Home);
        assert!(
            sel.write_pin,
            "an ignored pin (naming a now-ineligible host) must write a fresh pin at home"
        );
    }

    #[test]
    fn pin_for_unknown_host_ignored() {
        // E19: pin set but no key and the pinned host isn't eligible either
        // — falls all the way through to keyless BestOfTwo.
        let scores = vec![("hostA".to_string(), 0.1), ("hostB".to_string(), 0.2)];
        let mut rng = StdRng::seed_from_u64(7);
        let sel = select(None, Some("ghost"), &scores, &mut rng).unwrap();
        assert_eq!(sel.selection, Selection::BestOfTwo);
        assert_eq!(sel.host, "hostA");
        assert_eq!(sel.home, None);
        assert!(!sel.write_pin);
    }

    #[test]
    fn keyless_uses_best_of_two() {
        // With exactly two eligible hosts, BestOfTwo always samples both,
        // so the lower-scoring one is deterministic regardless of seed.
        let scores = vec![("hostA".to_string(), 1.0), ("hostB".to_string(), 2.0)];
        for seed in 0..10u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let sel = select(None, None, &scores, &mut rng).unwrap();
            assert_eq!(sel.host, "hostA");
            assert_eq!(sel.selection, Selection::BestOfTwo);
            assert_eq!(sel.home, None);
            assert!(!sel.write_pin);
        }
    }

    #[test]
    fn keyless_best_of_two_with_four_hosts_is_not_always_global_min() {
        // With 4+ hosts, BestOfTwo samples only 2 of them, so the pick is
        // the lower-scored of the *sampled* pair — not necessarily the
        // fleet-wide minimum. Assert both properties across seeds: every
        // pick is a valid (sampled-pair) minimum, and at least one seed
        // picks something other than the global min ("hostA").
        let scores = vec![
            ("hostA".to_string(), 0.1), // global min
            ("hostB".to_string(), 0.5),
            ("hostC".to_string(), 0.6),
            ("hostD".to_string(), 0.7),
        ];
        let by_host: HashMap<&str, f64> = scores.iter().map(|(h, s)| (h.as_str(), *s)).collect();

        let mut saw_non_global_min = false;
        for seed in 0..50u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let sel = select(None, None, &scores, &mut rng).unwrap();
            assert_eq!(sel.selection, Selection::BestOfTwo);
            assert!(
                by_host.contains_key(sel.host.as_str()),
                "picked host must be one of the eligible hosts"
            );
            if sel.host != "hostA" {
                saw_non_global_min = true;
            }
        }
        assert!(
            saw_non_global_min,
            "with 4 hosts, best-of-two should sometimes miss the global min"
        );
    }

    #[test]
    fn hrw_rank_is_input_order_independent() {
        let key = AffinityKey::from_bytes([11u8; 16]);
        let hosts = ["gpu01", "gpu02", "gpu03", "gpu04"];
        let mut reversed = hosts;
        reversed.reverse();
        assert_eq!(hrw_rank(&key, &hosts), hrw_rank(&key, &reversed));
    }

    #[test]
    fn keyless_single_host_is_taken() {
        let scores = vec![("hostA".to_string(), 1.0)];
        let mut rng = StdRng::seed_from_u64(1);
        let sel = select(None, None, &scores, &mut rng).unwrap();
        assert_eq!(sel.host, "hostA");
        assert_eq!(sel.selection, Selection::BestOfTwo);
    }

    #[test]
    fn hrw_rank_is_deterministic_and_stable_under_removal() {
        let key = AffinityKey::from_bytes([7u8; 16]);
        let hosts = ["gpu01", "gpu02", "gpu03", "gpu04"];
        let full = hrw_rank(&key, &hosts);
        assert_eq!(full, hrw_rank(&key, &hosts), "deterministic for same input");

        // Remove one host (not necessarily the top-ranked) and check the
        // relative order of the rest is preserved.
        let removed = full[full.len() / 2].clone();
        let remaining: Vec<&str> = hosts.iter().copied().filter(|h| *h != removed).collect();
        let reduced = hrw_rank(&key, &remaining);
        let expected: Vec<String> = full.into_iter().filter(|h| *h != removed).collect();
        assert_eq!(reduced, expected);
    }

    #[test]
    fn affinity_key_hex_round_trips() {
        let key = AffinityKey::from_bytes([0xabu8; 16]);
        let hex = key.to_hex();
        assert_eq!(hex.len(), 32);
        assert_eq!(hex, "ab".repeat(16));
        let decoded = AffinityKey::from_hex(&hex).expect("valid hex decodes");
        assert_eq!(decoded.to_hex(), hex);
    }

    #[test]
    fn affinity_key_from_hex_rejects_wrong_length_and_garbage() {
        assert!(AffinityKey::from_hex("abcd").is_none());
        assert!(AffinityKey::from_hex("not-hex-at-all-not-hex-at-all!!").is_none());
    }

    #[test]
    fn pin_id_is_deterministic_and_key_dependent() {
        let secret = [9u8; 32];
        let key1 = AffinityKey::from_bytes([1u8; 16]);
        let key2 = AffinityKey::from_bytes([2u8; 16]);
        let id1 = pin_id(&key1, &secret);
        let id1_again = pin_id(&key1, &secret);
        let id2 = pin_id(&key2, &secret);
        assert_eq!(id1.as_bytes(), id1_again.as_bytes());
        assert_ne!(id1.as_bytes(), id2.as_bytes());
        assert_eq!(id1.to_hex().len(), 32);
    }

    #[test]
    fn pin_table_expires_by_ttl() {
        use crate::consts::PIN_TTL_MS;
        let mut table = PinTable::default();
        let id = pin_id(&AffinityKey::from_bytes([3u8; 16]), &[1u8; 32]);
        table.insert(*id.as_bytes(), "gpu09".to_string(), 1_000);
        assert_eq!(table.get(&id, 1_000), Some(("gpu09", 1_000)));
        assert_eq!(
            table.get(&id, 1_000 + PIN_TTL_MS - 1),
            Some(("gpu09", 1_000))
        );
        assert_eq!(table.get(&id, 1_000 + PIN_TTL_MS), None);
    }

    #[test]
    fn pin_table_prune_drops_only_expired() {
        use crate::consts::PIN_TTL_MS;
        let mut table = PinTable::default();
        table.insert([1u8; 16], "old".to_string(), 1_000);
        table.insert([2u8; 16], "new".to_string(), 5_000);
        table.prune(1_000 + PIN_TTL_MS);
        assert_eq!(table.len(), 1);
        let new_id = PinId([2u8; 16]);
        assert_eq!(table.get(&new_id, 1_000 + PIN_TTL_MS), Some(("new", 5_000)));
    }
}
