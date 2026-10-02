//! The placement knobs that can change at runtime. Everything else in
//! `consts.rs` stays a constant. The defaults are the constants, so a process
//! that never loads a setting behaves exactly as the constants say.

use crate::consts::{
    AFFINITY_ABS_SLACK, AFFINITY_EPS, KV_MAX, LANE_LOAD_TOKENS, PIN_HOLD_FACTOR, PIN_TTL_MS,
    PLACEMENT_ENABLED,
};

/// Live-tunable placement knobs. `Copy`, so a decision reads it once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// Absolute slack of the affinity bound (`AFFINITY_ABS_SLACK`).
    pub affinity_abs_slack: f64,
    /// Relative tolerance of the affinity bound (`AFFINITY_EPS`).
    pub affinity_eps: f64,
    /// KV usage at or above which a replica is excluded (`KV_MAX`).
    pub kv_max: f64,
    /// Load at or above which a replica is a heavy-lane member
    /// (`LANE_LOAD_TOKENS`).
    pub lane_load_tokens: u64,
    /// Scale on the prompt's cold-prefill cost in the pin-hold test
    /// (`PIN_HOLD_FACTOR`).
    pub pin_hold_factor: f64,
    /// How long a follow pin stays valid, in ms (`PIN_TTL_MS`).
    pub pin_ttl_ms: u64,
    /// False routes every request through legacy routing
    /// (`LegacyReason::Disabled`) (`PLACEMENT_ENABLED`).
    pub enabled: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            affinity_abs_slack: AFFINITY_ABS_SLACK,
            affinity_eps: AFFINITY_EPS,
            kv_max: KV_MAX,
            lane_load_tokens: LANE_LOAD_TOKENS,
            pin_hold_factor: PIN_HOLD_FACTOR,
            pin_ttl_ms: PIN_TTL_MS,
            enabled: PLACEMENT_ENABLED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_constants() {
        let t = Tuning::default();
        assert_eq!(t.affinity_abs_slack, 0.25);
        assert_eq!(t.affinity_eps, AFFINITY_EPS);
        assert_eq!(t.kv_max, KV_MAX);
        assert_eq!(t.lane_load_tokens, LANE_LOAD_TOKENS);
        assert_eq!(t.pin_hold_factor, 1.0);
        assert_eq!(t.pin_ttl_ms, PIN_TTL_MS);
        assert!(t.enabled);
        assert_eq!(AFFINITY_ABS_SLACK, 0.25);
    }
}
