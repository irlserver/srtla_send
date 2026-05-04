//! Per-link congestion-control soft cap (shadow mode).
//!
//! A small per-connection state machine that produces a `target_bps` —
//! a soft cap on the rate the scheduler should push down this link. The
//! cap is **not consumed** by selection yet; it's emitted via stats so
//! we can compare its decisions against actual selection outcomes
//! during a soak window. Wire in as an admission gate on Enhanced
//! selection only after the soak.
//!
//! ## State machine
//!
//! Three states cover the practical regimes for a SRTLA soft cap:
//!
//! - **Climbing**: RTT stable, no loss observed in the recent window.
//!   Additively grow `target_bps`. Step is bounded by current cap and
//!   the link's measured throughput so it doesn't run away on idle
//!   links.
//! - **Holding**: RTT inflating but no loss yet (delay-based signal of
//!   approaching congestion). Hold target, don't grow.
//! - **BackingOff**: Loss observed (NAK rate up). Multiplicative
//!   decrease.
//!
//! Three states cover the steady-state, the bufferbloat-onset state,
//! and the loss state — which is what matters for a soft cap.
//!
//! ## Age-bucketed RTT EWMA
//!
//! EWMA weight banded by time-since-last-sample to stay responsive
//! after stale periods. Power-of-2 ratios `1:1, 1:4, 1:8, 1:16` at age
//! bands `>= 2s, >= 1s, >= 500ms, >= 250ms`. After 2s with no sample we
//! reset to the new sample verbatim.
//!
//! All numbers here are starting points; soak data may suggest
//! retuning.

use std::collections::HashMap;

use crate::connection::SrtlaConnection;

/// Sliding-window length for the loss-permille tracker, in
/// milliseconds. 1s matches the rough timescale of NAK feedback.
const LOSS_WINDOW_MS: u64 = 1_000;

/// Loss-permille threshold above which we declare a backoff regime.
/// 5 parts-per-thousand = 0.5%.
const LOSS_BACKOFF_PERMILLE: u32 = 5;

/// Multiplicative-decrease factor (permille). 0.85 = -15%.
const BACKOFF_PERMILLE: u32 = 850;

/// Climbing additive-increase step as a permille of the current target.
/// 0.02 = +2% per tick.
const AI_STEP_PERMILLE: u32 = 20;

/// Above this RTT-inflation factor (relative to the link's minimum
/// observed RTT) we declare a hold regime even when no loss has hit.
/// 1.5 = "RTT is 50% above the floor".
const RTT_HOLD_FACTOR: f64 = 1.5;

/// Floor for `target_bps`. Below this we don't bother modulating.
const MIN_TARGET_BPS: u64 = 100_000;

/// Ceiling we never let `target_bps` exceed before measured traffic
/// catches up. Soft cap; tuning starts here, may be revised.
const MAX_TARGET_BPS: u64 = 200_000_000;

/// Initial target on first sample. Conservative on purpose.
const INITIAL_TARGET_BPS: u64 = 1_000_000;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum CcState {
    /// Pre-RTT-sample bootstrap state. Target stays at the floor until
    /// the first RTT update arrives.
    #[default]
    Bootstrap,
    /// RTT stable, no loss. Additive increase.
    Climbing,
    /// RTT inflating, no loss yet. Hold target.
    Holding,
    /// Loss observed. Multiplicative decrease.
    BackingOff,
}

impl CcState {
    pub fn as_str(self) -> &'static str {
        match self {
            CcState::Bootstrap => "bootstrap",
            CcState::Climbing => "climbing",
            CcState::Holding => "holding",
            CcState::BackingOff => "backing_off",
        }
    }
}

/// One sample of `(timestamp_ms, lost_packets)`. Used for the sliding
/// loss-permille window.
#[derive(Copy, Clone, Debug)]
struct LossSample {
    ts_ms: u64,
    lost: u32,
    sent: u32,
}

/// Per-connection CC state. One of these lives on each
/// `SrtlaConnection` (added in a follow-up patch). Today it's
/// instantiated next to the classifier filter and indexed by
/// `conn_id`.
#[derive(Debug)]
pub struct LinkCongestionState {
    pub state: CcState,
    pub target_bps: u64,
    /// Power-of-2 age-bucketed EWMA of RTT (ms).
    rtt_ewma_ms: f64,
    /// Variance proxy: EWMA of `|sample - rtt_ewma|` with weight 1:3.
    rtt_var_ms: f64,
    /// Lowest RTT we've ever seen on this link. Used to detect
    /// inflation.
    rtt_min_ms: f64,
    /// Wall-clock of the last RTT update.
    last_rtt_update_ms: u64,
    /// Sliding-window loss samples.
    loss_samples: Vec<LossSample>,
    /// Aggregated within the window.
    window_lost: u32,
    window_sent: u32,
}

impl Default for LinkCongestionState {
    fn default() -> Self {
        Self {
            state: CcState::Bootstrap,
            target_bps: MIN_TARGET_BPS,
            rtt_ewma_ms: 0.0,
            rtt_var_ms: 0.0,
            rtt_min_ms: f64::INFINITY,
            last_rtt_update_ms: 0,
            loss_samples: Vec::new(),
            window_lost: 0,
            window_sent: 0,
        }
    }
}

impl LinkCongestionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed an RTT sample. Updates the age-bucketed EWMA, variance
    /// proxy, and minimum.
    pub fn record_rtt(&mut self, rtt_ms: f64, now_ms: u64) {
        if !rtt_ms.is_finite() || rtt_ms <= 0.0 {
            return;
        }
        let age_ms = now_ms.saturating_sub(self.last_rtt_update_ms);

        // Age-bucketed EWMA weight (new : old).
        // Bands: >=2s reset, >=1s 1:1, >=500ms 1:4, >=250ms 1:8,
        // <250ms 1:16. First sample (rtt_ewma == 0) snaps verbatim —
        // we don't gate on `last_rtt_update_ms == 0` because legitimate
        // samples may arrive at t=0 in tests / monotonic-clock startup.
        let new_w = if self.rtt_ewma_ms == 0.0 || age_ms >= 2_000 {
            self.rtt_ewma_ms = rtt_ms;
            self.rtt_var_ms = 0.0;
            self.last_rtt_update_ms = now_ms;
            self.rtt_min_ms = self.rtt_min_ms.min(rtt_ms);
            return;
        } else if age_ms >= 1_000 {
            (1.0, 1.0)
        } else if age_ms >= 500 {
            (1.0, 4.0)
        } else if age_ms >= 250 {
            (1.0, 8.0)
        } else {
            (1.0, 16.0)
        };
        let (w_new, w_old) = new_w;
        let denom = w_new + w_old;
        let prev = self.rtt_ewma_ms;
        self.rtt_ewma_ms = (rtt_ms * w_new + prev * w_old) / denom;
        // Variance proxy: 1:3 weighted moving average of |dev|.
        let dev = (rtt_ms - prev).abs();
        self.rtt_var_ms = (dev * 1.0 + self.rtt_var_ms * 3.0) / 4.0;
        self.rtt_min_ms = self.rtt_min_ms.min(rtt_ms);
        self.last_rtt_update_ms = now_ms;
    }

    /// Feed a (sent, lost) sample. Sliding-window aggregates evict
    /// entries older than `LOSS_WINDOW_MS`.
    ///
    /// Not yet wired into production: NAK delta plumbing arrives in a
    /// follow-up commit. Tests exercise it directly so the algorithm
    /// can be validated independently.
    #[allow(dead_code)]
    pub fn record_loss(&mut self, sent: u32, lost: u32, now_ms: u64) {
        self.loss_samples.push(LossSample {
            ts_ms: now_ms,
            sent,
            lost,
        });
        self.window_sent = self.window_sent.saturating_add(sent);
        self.window_lost = self.window_lost.saturating_add(lost);
        self.evict_expired(now_ms);
    }

    fn evict_expired(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(LOSS_WINDOW_MS);
        while let Some(front) = self.loss_samples.first() {
            if front.ts_ms < cutoff {
                self.window_sent = self.window_sent.saturating_sub(front.sent);
                self.window_lost = self.window_lost.saturating_sub(front.lost);
                self.loss_samples.remove(0);
            } else {
                break;
            }
        }
    }

    /// Current loss permille over the window.
    pub fn loss_permille(&self) -> u32 {
        if self.window_sent == 0 {
            return 0;
        }
        let permille = (self.window_lost as u64).saturating_mul(1_000) / (self.window_sent as u64);
        permille.min(1_000_000) as u32
    }

    /// Recompute the state and `target_bps` from the latest signals.
    /// Called once per housekeeping tick.
    pub fn tick(&mut self, observed_bps: u64, now_ms: u64) {
        self.evict_expired(now_ms);

        if !self.rtt_ewma_ms.is_finite() || self.rtt_ewma_ms == 0.0 {
            // No RTT yet: stay in bootstrap, hold the floor.
            self.state = CcState::Bootstrap;
            self.target_bps = MIN_TARGET_BPS;
            return;
        }

        let loss_pm = self.loss_permille();
        let rtt_inflation = if self.rtt_min_ms.is_finite() && self.rtt_min_ms > 0.0 {
            self.rtt_ewma_ms / self.rtt_min_ms
        } else {
            1.0
        };

        let next_state = if loss_pm > LOSS_BACKOFF_PERMILLE {
            CcState::BackingOff
        } else if rtt_inflation > RTT_HOLD_FACTOR {
            CcState::Holding
        } else {
            CcState::Climbing
        };
        self.state = next_state;

        // First non-bootstrap tick: seed the target from observed throughput
        // (or a conservative floor if no traffic yet).
        if self.target_bps == MIN_TARGET_BPS {
            let seed = observed_bps.max(INITIAL_TARGET_BPS);
            self.target_bps = seed.clamp(MIN_TARGET_BPS, MAX_TARGET_BPS);
        }

        let prev = self.target_bps as f64;
        let next = match next_state {
            CcState::Bootstrap => prev,
            CcState::Climbing => {
                let step = (prev * AI_STEP_PERMILLE as f64) / 1000.0;
                // Don't grow more than 2x measured traffic — prevents
                // ramp on idle links.
                let measured_cap = (observed_bps as f64) * 2.0;
                let cap_above_measured = if observed_bps > 0 {
                    prev.max(MIN_TARGET_BPS as f64) + step.min(measured_cap - prev).max(0.0)
                } else {
                    prev + step
                };
                cap_above_measured
            }
            CcState::Holding => prev,
            CcState::BackingOff => (prev * BACKOFF_PERMILLE as f64) / 1000.0,
        };

        self.target_bps = (next as u64).clamp(MIN_TARGET_BPS, MAX_TARGET_BPS);
    }

    /// Convenience for stats emission.
    pub fn snapshot(&self) -> LinkCcSnapshot {
        LinkCcSnapshot {
            state: self.state,
            target_bps: self.target_bps,
            rtt_ewma_ms: self.rtt_ewma_ms,
            rtt_var_ms: self.rtt_var_ms,
            rtt_min_ms: if self.rtt_min_ms.is_finite() {
                self.rtt_min_ms
            } else {
                0.0
            },
            loss_permille: self.loss_permille(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct LinkCcSnapshot {
    pub state: CcState,
    pub target_bps: u64,
    pub rtt_ewma_ms: f64,
    pub rtt_var_ms: f64,
    pub rtt_min_ms: f64,
    pub loss_permille: u32,
}

/// Owns one [`LinkCongestionState`] per connection. Driven by the
/// sender's housekeeping tick: `tick_all` reads each connection's
/// current RTT and bitrate, feeds the per-link state, and produces
/// snapshots for the stats exporter.
///
/// Loss is fed separately on the NAK path (not yet wired) — for now
/// `record_loss` stays at zero, which keeps every link in the
/// climbing/holding regimes. Plumbing the NAK delta in is a follow-up
/// commit.
#[derive(Default)]
pub struct LinkCcController {
    per_conn: HashMap<u64, LinkCongestionState>,
}

impl LinkCcController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update each connection's CC state from the latest signals.
    /// Returns a per-conn snapshot map keyed by `conn_id` for stats
    /// emission.
    pub fn tick_all(
        &mut self,
        connections: &[SrtlaConnection],
        now_ms: u64,
    ) -> HashMap<u64, LinkCcSnapshot> {
        let mut alive: HashMap<u64, LinkCcSnapshot> = HashMap::with_capacity(connections.len());
        for conn in connections {
            let entry = self
                .per_conn
                .entry(conn.conn_id)
                .or_insert_with(LinkCongestionState::new);
            let rtt_ms = conn.get_smooth_rtt_ms();
            if rtt_ms > 0.0 {
                entry.record_rtt(rtt_ms, now_ms);
            }
            let observed_bps = conn.bitrate.current_bitrate_bps.max(0.0) as u64;
            entry.tick(observed_bps, now_ms);
            alive.insert(conn.conn_id, entry.snapshot());
        }
        // Garbage-collect entries for connections that disappeared.
        self.per_conn.retain(|id, _| alive.contains_key(id));
        alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_holds_floor() {
        let mut cc = LinkCongestionState::new();
        cc.tick(0, 0);
        assert_eq!(cc.state, CcState::Bootstrap);
        assert_eq!(cc.target_bps, MIN_TARGET_BPS);
    }

    #[test]
    fn climbing_grows_target() {
        let mut cc = LinkCongestionState::new();
        cc.record_rtt(50.0, 1_000);
        cc.tick(2_000_000, 1_000);
        assert_eq!(cc.state, CcState::Climbing);
        let first = cc.target_bps;
        cc.tick(2_000_000, 1_100);
        assert!(cc.target_bps >= first);
    }

    #[test]
    fn holding_when_rtt_inflates() {
        let mut cc = LinkCongestionState::new();
        // Establish low baseline.
        cc.record_rtt(20.0, 0);
        cc.tick(2_000_000, 0);

        // Sustained inflation — feed enough samples for the EWMA to
        // climb past 1.5x the min. The smoothing is intentionally slow
        // for single-sample spikes (that's what the EWMA is for).
        for i in 1..=10 {
            cc.record_rtt(60.0, i * 600);
            cc.tick(2_000_000, i * 600);
        }
        assert_eq!(cc.state, CcState::Holding);
    }

    #[test]
    fn backing_off_on_loss() {
        let mut cc = LinkCongestionState::new();
        cc.record_rtt(50.0, 0);
        cc.tick(2_000_000, 0);
        let before = cc.target_bps;

        // Lose 1% of packets — well above LOSS_BACKOFF_PERMILLE.
        cc.record_loss(1_000, 100, 100);
        cc.tick(2_000_000, 100);
        assert_eq!(cc.state, CcState::BackingOff);
        assert!(cc.target_bps < before);
    }

    #[test]
    fn loss_window_evicts() {
        let mut cc = LinkCongestionState::new();
        cc.record_loss(1_000, 100, 0);
        assert_eq!(cc.loss_permille(), 100);
        // Beyond window — should evict.
        cc.record_loss(0, 0, LOSS_WINDOW_MS + 10);
        assert_eq!(cc.loss_permille(), 0);
    }

    #[test]
    fn rtt_ewma_resets_after_2s_gap() {
        let mut cc = LinkCongestionState::new();
        cc.record_rtt(50.0, 0);
        // 2.5s later — should snap to the new sample.
        cc.record_rtt(200.0, 2_500);
        assert!((cc.rtt_ewma_ms - 200.0).abs() < 0.01);
    }

    #[test]
    fn rtt_ewma_weights_by_age() {
        let mut cc = LinkCongestionState::new();
        cc.record_rtt(100.0, 0);
        // Within 250ms — heavy weight on old (1:16).
        cc.record_rtt(200.0, 100);
        assert!(cc.rtt_ewma_ms < 110.0);
    }
}
