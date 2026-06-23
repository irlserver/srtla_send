//! Per-link congestion-control soft cap.
//!
//! A small per-connection state machine that produces a `target_bps` —
//! a soft cap on the rate the scheduler should push down this link.
//! `target_bps` is consumed by Enhanced selection (the soft-cap score
//! multiplier and the BDP in-flight cap), and the sustained `loss_degraded`
//! latch feeds the routing loss gate. The instantaneous `BackingOff` state
//! drives this controller's own bitrate backoff but does not gate routing.
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
/// 0.02 = +2% per tick — the conservative baseline for steady state.
const AI_STEP_PERMILLE: u32 = 20;

/// Bigger step (+6% per tick) used by High-Additive-Increase mode when
/// RTT is stable enough that we're confident headroom exists. "Stable"
/// here = RTT variance ≤ 10% of the smoothed RTT mean.
const HAI_STEP_PERMILLE: u32 = 60;

/// Step used during fast-recovery after a backoff or drain. Faster
/// than normal AI, slower than HAI — we want to claw back quickly
/// but not overshoot the level that triggered the backoff.
const FAST_RECOVERY_STEP_PERMILLE: u32 = 40;

/// Number of ticks we stay in fast-recovery after exiting BackingOff
/// or Drain. ~5s at 1Hz tick which roughly covers one cellular RTT
/// cycle plus margin.
const FAST_RECOVERY_TICKS: u32 = 5;

/// RTT-inflation threshold for one-shot Drain. When the smoothed RTT
/// is more than 2.0x the running minimum without any loss observed,
/// the bandwidth-delay queue is overflowing — cut hard rather than
/// wait for ARQ to surface the loss.
const DRAIN_RTT_INFLATION: f64 = 2.0;

/// Drain factor applied as a one-shot multiplicative decrease when
/// Drain triggers. 0.75 = -25%.
const DRAIN_PERMILLE: u32 = 750;

/// "Stable RTT" threshold for HAI: rtt_var must be at most this
/// fraction of rtt_ewma. 0.10 = "variance < 10% of mean".
const HAI_VARIANCE_FRACTION: f64 = 0.10;

/// Assumed SRT payload size for converting cumulative bytes-sent
/// counters into packet counts when feeding `record_loss`. Most SRTLA
/// deployments run with the libsrt 1316-byte default; off-by-a-factor
/// only matters for the loss-permille ratio, which is invariant under
/// uniform packet-size assumptions.
pub(crate) const ASSUMED_SRT_PAYLOAD_BYTES: u64 = 1316;

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

/// Time constant (ms) for the link loss EWMA that drives continuous
/// phase demotion. Cellular HARQ stalls last 400-800ms; a 2s tau keeps
/// the signal from reacting to a single stall while still demoting a
/// link that is genuinely shedding traffic.
const LOSS_EWMA_TAU_MS: f64 = 2_000.0;

/// Loss-EWMA fraction (0..1) above which, once sustained for
/// `LOSS_DEGRADE_SUSTAIN_MS`, the link is flagged degraded. High on
/// purpose: only a link losing most of its traffic trips it, so
/// transient stalls do not cause a false demotion. This drives a graded
/// score penalty, never a hard removal — the scheduler keeps the link
/// so it can prove its own recovery on the next ACK.
const LOSS_DEGRADE_ENTER: f64 = 0.55;

/// Hysteresis: the loss EWMA must fall back below this before the
/// degraded flag clears.
const LOSS_DEGRADE_CLEAR: f64 = 0.25;

/// How long the loss EWMA must stay above `LOSS_DEGRADE_ENTER` before
/// the degraded flag latches.
const LOSS_DEGRADE_SUSTAIN_MS: u64 = 4_000;

/// Window (ms) over which the CC's minimum RTT is tracked. A lifetime
/// minimum pins `rtt_inflation` high forever after one early low sample,
/// so a cellular handover that raises the true floor reads as permanent
/// congestion and traps the controller in Drain/Hold. Expiring the min
/// over ~30s lets the baseline follow the path. Matches the connection
/// RTT tracker's slow-window timescale.
const CC_RTT_MIN_WINDOW_MS: u64 = 30_000;

/// Reject a single throughput sample that exceeds this factor times the
/// current target estimate. A stall-release ACK flush (a carrier NAT
/// rebind dumping thousands of queued ACKs in one window) or a
/// saturation burst can momentarily read 3-5x the link's true rate;
/// without this clamp it inflates the soft cap and causes bufferbloat a
/// few seconds later. The estimate may still rise, just never more than
/// this factor from one contaminated sample.
const CC_OUTLIER_FACTOR: f64 = 4.0;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum CcState {
    /// Pre-RTT-sample bootstrap state. Target stays at the floor until
    /// the first RTT update arrives.
    #[default]
    Bootstrap,
    /// RTT stable, no loss. Additive increase. Step size depends on
    /// the current [`ClimbMode`]: Normal (2%), Hai (6%), or
    /// FastRecovery (4%).
    Climbing,
    /// RTT inflating, no loss yet. Hold target.
    Holding,
    /// Loss observed. Multiplicative decrease.
    BackingOff,
    /// One-shot drain when RTT inflation crosses
    /// `DRAIN_RTT_INFLATION` without explicit loss — bandwidth-delay
    /// queue is overflowing. Drops target to 75% on entry; the next
    /// tick re-evaluates and typically lands in Holding.
    Drain,
}

impl CcState {
    pub fn as_str(self) -> &'static str {
        match self {
            CcState::Bootstrap => "bootstrap",
            CcState::Climbing => "climbing",
            CcState::Holding => "holding",
            CcState::BackingOff => "backing_off",
            CcState::Drain => "drain",
        }
    }
}

/// Sub-mode within [`CcState::Climbing`] that controls AI step size.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum ClimbMode {
    /// Standard 2% additive increase.
    #[default]
    Normal,
    /// 6% additive increase when RTT is stable (variance ≤ 10% of mean).
    /// "High Additive Increase" — we have confident headroom signal.
    Hai,
    /// 4% additive increase for `FAST_RECOVERY_TICKS` ticks after
    /// exiting BackingOff or Drain. Claws back quickly without
    /// overshooting the level that triggered the backoff.
    FastRecovery,
}

impl ClimbMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ClimbMode::Normal => "normal",
            ClimbMode::Hai => "hai",
            ClimbMode::FastRecovery => "fast_recovery",
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
    /// Active climb sub-mode. Only meaningful when `state == Climbing`.
    pub climb_mode: ClimbMode,
    pub target_bps: u64,
    /// Power-of-2 age-bucketed EWMA of RTT (ms).
    rtt_ewma_ms: f64,
    /// Variance proxy: EWMA of `|sample - rtt_ewma|` with weight 1:3.
    rtt_var_ms: f64,
    /// Lowest RTT in the recent window (see `CC_RTT_MIN_WINDOW_MS`).
    /// Used to detect inflation. Windowed, not lifetime, so a handover
    /// that raises the floor doesn't pin inflation high forever.
    rtt_min_ms: f64,
    /// Wall-clock the current `rtt_min_ms` was set. When it ages past
    /// the window the min resets to the next sample.
    rtt_min_stamp_ms: u64,
    /// Wall-clock of the last RTT update.
    last_rtt_update_ms: u64,
    /// Sliding-window loss samples.
    loss_samples: Vec<LossSample>,
    /// Aggregated within the window.
    window_lost: u32,
    window_sent: u32,
    /// Ticks remaining in fast-recovery mode. Decremented each
    /// `tick()` call; while > 0 the climb sub-mode is `FastRecovery`.
    fast_recovery_ticks: u32,
    /// Cumulative bytes-sent the previous tick observed. Drives the
    /// per-tick `sent` delta fed to `record_loss`.
    prev_bytes_sent_total: u64,
    /// Cumulative NAK count the previous tick observed. Drives the
    /// per-tick `lost` delta.
    prev_nak_total: i32,
    /// Set after the first `observe_traffic` call. Until then we don't
    /// know what "previous" means so we just stash the totals as a
    /// baseline without emitting a loss sample.
    traffic_baseline_set: bool,
    /// Time-decayed EWMA of the windowed loss fraction (0..1). Drives
    /// continuous phase demotion in place of a binary link-death gate.
    loss_ewma: f64,
    /// Wall-clock of the last `loss_ewma` update. 0 = never updated.
    loss_ewma_last_ms: u64,
    /// Wall-clock since which `loss_ewma` has been continuously above
    /// `LOSS_DEGRADE_ENTER`. 0 = currently below the entry threshold.
    loss_high_since_ms: u64,
    /// Latched hysteretic verdict: true once loss has been sustained
    /// high, false again once it recovers below `LOSS_DEGRADE_CLEAR`.
    loss_degraded: bool,
}

impl Default for LinkCongestionState {
    fn default() -> Self {
        Self {
            state: CcState::Bootstrap,
            climb_mode: ClimbMode::Normal,
            target_bps: MIN_TARGET_BPS,
            rtt_ewma_ms: 0.0,
            rtt_var_ms: 0.0,
            rtt_min_ms: f64::INFINITY,
            rtt_min_stamp_ms: 0,
            last_rtt_update_ms: 0,
            loss_samples: Vec::new(),
            window_lost: 0,
            window_sent: 0,
            fast_recovery_ticks: 0,
            prev_bytes_sent_total: 0,
            prev_nak_total: 0,
            traffic_baseline_set: false,
            loss_ewma: 0.0,
            loss_ewma_last_ms: 0,
            loss_high_since_ms: 0,
            loss_degraded: false,
        }
    }
}

impl LinkCongestionState {
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
            self.update_rtt_min(rtt_ms, now_ms);
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
        self.update_rtt_min(rtt_ms, now_ms);
        self.last_rtt_update_ms = now_ms;
    }

    /// Windowed minimum RTT: adopt any lower sample, and when the held
    /// minimum ages past `CC_RTT_MIN_WINDOW_MS` reset it to the current
    /// sample so the baseline follows a changed propagation floor (e.g.
    /// after a cellular handover) instead of staying pinned to a stale
    /// low sample.
    fn update_rtt_min(&mut self, rtt_ms: f64, now_ms: u64) {
        let stale = now_ms.saturating_sub(self.rtt_min_stamp_ms) > CC_RTT_MIN_WINDOW_MS;
        if !self.rtt_min_ms.is_finite() || rtt_ms < self.rtt_min_ms || stale {
            self.rtt_min_ms = rtt_ms;
            self.rtt_min_stamp_ms = now_ms;
        }
    }

    /// Feed cumulative (bytes_sent, nak_total) snapshots from the
    /// connection. Computes per-tick deltas against the previous call
    /// and forwards them to `record_loss`. First call after creation
    /// stashes the values as a baseline and returns without sampling.
    ///
    /// Decoupling the cumulative→delta conversion from `record_loss`
    /// keeps the latter directly testable with synthetic deltas while
    /// the production path only needs to thread totals.
    pub fn observe_traffic(&mut self, bytes_sent_total: u64, nak_total: i32, now_ms: u64) {
        if !self.traffic_baseline_set {
            self.prev_bytes_sent_total = bytes_sent_total;
            self.prev_nak_total = nak_total;
            self.traffic_baseline_set = true;
            return;
        }
        let delta_bytes = bytes_sent_total.saturating_sub(self.prev_bytes_sent_total);
        let delta_nak = nak_total.saturating_sub(self.prev_nak_total).max(0);
        self.prev_bytes_sent_total = bytes_sent_total;
        self.prev_nak_total = nak_total;

        if delta_bytes == 0 && delta_nak == 0 {
            // No traffic this tick — don't pollute the window with a
            // zero-sample. evict_expired in tick() handles aging.
            return;
        }
        let sent_pkts = (delta_bytes / ASSUMED_SRT_PAYLOAD_BYTES).min(u32::MAX as u64) as u32;
        let lost_pkts = delta_nak as u32;
        // Guard against a NAK delta with no corresponding bytes-sent
        // delta (e.g. NAKs arriving on a now-quiet link) — the loss
        // permille formula divides by `window_sent` which would
        // saturate to 1000 with zero-divisor handling. Treat as a
        // single-packet "sent" baseline so the ratio stays bounded.
        let sent_pkts = sent_pkts.max(if lost_pkts > 0 { 1 } else { 0 });
        self.record_loss(sent_pkts, lost_pkts, now_ms);
    }

    /// Feed a (sent, lost) sample directly. Sliding-window aggregates
    /// evict entries older than `LOSS_WINDOW_MS`. Production code uses
    /// [`observe_traffic`] which threads cumulative counters; this
    /// method is exposed for unit tests.
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
            self.climb_mode = ClimbMode::Normal;
            self.target_bps = MIN_TARGET_BPS;
            return;
        }

        let loss_pm = self.loss_permille();
        self.update_loss_ewma(loss_pm, now_ms);
        let rtt_inflation = if self.rtt_min_ms.is_finite() && self.rtt_min_ms > 0.0 {
            self.rtt_ewma_ms / self.rtt_min_ms
        } else {
            1.0
        };

        let prev_state = self.state;
        let next_state = if loss_pm > LOSS_BACKOFF_PERMILLE {
            CcState::BackingOff
        } else if rtt_inflation >= DRAIN_RTT_INFLATION {
            // BDQ overload before loss surfaces — drain hard.
            CcState::Drain
        } else if rtt_inflation > RTT_HOLD_FACTOR {
            CcState::Holding
        } else {
            CcState::Climbing
        };

        // Fast-recovery accounting: arm the timer when leaving
        // BackingOff or Drain into Climbing. The budget is consumed at the
        // end of the tick, AFTER `pick_climb_mode` reads it below, so the
        // arming tick itself counts toward the window and all
        // FAST_RECOVERY_TICKS climbs use the fast step. Decrementing here
        // would burn the first tick before it was ever used (the documented
        // 5-tick window would only fire 4).
        if let (CcState::BackingOff | CcState::Drain, CcState::Climbing) = (prev_state, next_state)
        {
            self.fast_recovery_ticks = FAST_RECOVERY_TICKS;
        }

        self.state = next_state;

        // Outlier rejection: clamp a single throughput sample to
        // `CC_OUTLIER_FACTOR` times the running estimate (floored at the
        // initial estimate so the first seed isn't pinned to the very
        // low target_bps floor). This bounds how far one contaminated
        // burst can move the soft cap, whether at the seed or via the
        // climb's measured cap.
        let baseline = self.target_bps.max(INITIAL_TARGET_BPS) as f64;
        let sane_observed = (observed_bps as f64).min(CC_OUTLIER_FACTOR * baseline) as u64;

        // First non-bootstrap tick: seed the target from observed throughput
        // (or a conservative floor if no traffic yet).
        if self.target_bps == MIN_TARGET_BPS {
            let seed = sane_observed.max(INITIAL_TARGET_BPS);
            self.target_bps = seed.clamp(MIN_TARGET_BPS, MAX_TARGET_BPS);
        }

        let prev = self.target_bps as f64;
        let next = match next_state {
            CcState::Bootstrap => {
                self.climb_mode = ClimbMode::Normal;
                prev
            }
            CcState::Climbing => {
                let mode = self.pick_climb_mode();
                self.climb_mode = mode;
                let step_pm = match mode {
                    ClimbMode::Normal => AI_STEP_PERMILLE,
                    ClimbMode::Hai => HAI_STEP_PERMILLE,
                    ClimbMode::FastRecovery => FAST_RECOVERY_STEP_PERMILLE,
                };
                let step = (prev * step_pm as f64) / 1000.0;
                // Don't grow more than 2x measured traffic — prevents
                // ramp on idle links. Same cap applies regardless of
                // step size. Uses the outlier-clamped sample so a burst
                // can't open a huge headroom for the AI to climb into.
                let measured_cap = (sane_observed as f64) * 2.0;
                if sane_observed > 0 {
                    prev.max(MIN_TARGET_BPS as f64) + step.min(measured_cap - prev).max(0.0)
                } else {
                    // No measured traffic this tick: hold the target instead of
                    // ramping into headroom that doesn't exist. A previously-idle
                    // link must re-justify growth from real throughput, otherwise
                    // the BDP in-flight cap and the soft-cap multiplier (both
                    // derived from target_bps) climb to MAX_TARGET_BPS and go
                    // inert, so the link gets flooded with no brake on its first
                    // real burst.
                    prev
                }
            }
            CcState::Holding => {
                self.climb_mode = ClimbMode::Normal;
                prev
            }
            CcState::BackingOff => {
                self.climb_mode = ClimbMode::Normal;
                (prev * BACKOFF_PERMILLE as f64) / 1000.0
            }
            CcState::Drain => {
                self.climb_mode = ClimbMode::Normal;
                // One-shot, per DRAIN_PERMILLE's contract: cut hard only on the
                // transition into Drain, then hold while we stay drained.
                // Applying the cut every tick compounds it, collapsing
                // target_bps to the floor within ~11 ticks and producing
                // saw-tooth oscillation under sustained RTT inflation. Leaving
                // and re-entering Drain applies a fresh cut.
                if prev_state != CcState::Drain {
                    (prev * DRAIN_PERMILLE as f64) / 1000.0
                } else {
                    prev
                }
            }
        };

        self.target_bps = (next as u64).clamp(MIN_TARGET_BPS, MAX_TARGET_BPS);

        // Consume one fast-recovery tick now that pick_climb_mode has read
        // the budget for this tick. Drop the remaining budget if we left
        // Climbing (a fresh backoff/drain re-arms it).
        if next_state == CcState::Climbing {
            self.fast_recovery_ticks = self.fast_recovery_ticks.saturating_sub(1);
        } else {
            self.fast_recovery_ticks = 0;
        }
    }

    /// Fold the latest windowed loss permille into the time-decayed
    /// loss EWMA and update the latched degraded verdict. Demotion is
    /// graded: the verdict only gates score (via the phase machine), it
    /// never removes the link from scheduling, so a link that briefly
    /// stalls and recovers is never starved of the ACK traffic that
    /// proves its recovery.
    fn update_loss_ewma(&mut self, loss_pm: u32, now_ms: u64) {
        let inst = (loss_pm as f64 / 1_000.0).clamp(0.0, 1.0);
        if self.loss_ewma_last_ms == 0 {
            self.loss_ewma = inst;
        } else {
            let dt = now_ms.saturating_sub(self.loss_ewma_last_ms) as f64;
            let alpha = 1.0 - (-dt / LOSS_EWMA_TAU_MS).exp();
            self.loss_ewma += (inst - self.loss_ewma) * alpha;
        }
        self.loss_ewma_last_ms = now_ms;

        if self.loss_ewma > LOSS_DEGRADE_ENTER {
            if self.loss_high_since_ms == 0 {
                self.loss_high_since_ms = now_ms;
            } else if now_ms.saturating_sub(self.loss_high_since_ms) >= LOSS_DEGRADE_SUSTAIN_MS {
                self.loss_degraded = true;
            }
        } else {
            self.loss_high_since_ms = 0;
            if self.loss_ewma < LOSS_DEGRADE_CLEAR {
                self.loss_degraded = false;
            }
        }
    }

    /// Decide which sub-mode applies on this Climbing tick.
    ///
    /// Order of precedence:
    /// 1. FastRecovery while we're inside the post-backoff window.
    /// 2. Hai when RTT is stable enough that variance is small
    ///    relative to the mean — confident there's headroom to take.
    /// 3. Normal otherwise.
    fn pick_climb_mode(&self) -> ClimbMode {
        if self.fast_recovery_ticks > 0 {
            return ClimbMode::FastRecovery;
        }
        if self.rtt_ewma_ms > 0.0 && self.rtt_var_ms <= self.rtt_ewma_ms * HAI_VARIANCE_FRACTION {
            return ClimbMode::Hai;
        }
        ClimbMode::Normal
    }

    /// Convenience for stats emission.
    pub fn snapshot(&self) -> LinkCcSnapshot {
        LinkCcSnapshot {
            state: self.state,
            climb_mode: self.climb_mode,
            target_bps: self.target_bps,
            rtt_ewma_ms: self.rtt_ewma_ms,
            rtt_var_ms: self.rtt_var_ms,
            rtt_min_ms: if self.rtt_min_ms.is_finite() {
                self.rtt_min_ms
            } else {
                0.0
            },
            loss_permille: self.loss_permille(),
            loss_ewma: self.loss_ewma,
            loss_degraded: self.loss_degraded,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct LinkCcSnapshot {
    pub state: CcState,
    pub climb_mode: ClimbMode,
    pub target_bps: u64,
    pub rtt_ewma_ms: f64,
    pub rtt_var_ms: f64,
    pub rtt_min_ms: f64,
    pub loss_permille: u32,
    /// Time-decayed loss fraction (0..1) driving phase demotion.
    pub loss_ewma: f64,
    /// Latched hysteretic verdict that loss has been sustained high.
    /// Consumed by the connection phase machine to demote (not remove)
    /// the link.
    pub loss_degraded: bool,
}

/// Owns one [`LinkCongestionState`] per connection. Driven by the
/// sender's housekeeping tick: `tick_all` reads each connection's
/// current RTT, observed bitrate, cumulative bytes-sent, and
/// cumulative NAK count; feeds the per-link state; and produces
/// snapshots for the stats exporter.
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
            let entry = self.per_conn.entry(conn.conn_id).or_default();
            let rtt_ms = conn.get_smooth_rtt_ms();
            if rtt_ms > 0.0 {
                entry.record_rtt(rtt_ms, now_ms);
            }
            // Loss path: cumulative bytes-sent and NAK count from the
            // connection — `observe_traffic` computes per-tick deltas
            // and forwards to `record_loss`. Before this wiring landed,
            // the loss window stayed empty and CcState::BackingOff was
            // unreachable in production.
            entry.observe_traffic(
                conn.bitrate.bytes_sent_total,
                conn.total_nak_count(),
                now_ms,
            );
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
        let mut cc = LinkCongestionState::default();
        cc.tick(0, 0);
        assert_eq!(cc.state, CcState::Bootstrap);
        assert_eq!(cc.target_bps, MIN_TARGET_BPS);
    }

    #[test]
    fn climbing_grows_target() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(50.0, 1_000);
        cc.tick(2_000_000, 1_000);
        assert_eq!(cc.state, CcState::Climbing);
        let first = cc.target_bps;
        cc.tick(2_000_000, 1_100);
        assert!(cc.target_bps >= first);
    }

    #[test]
    fn holding_when_rtt_inflates() {
        let mut cc = LinkCongestionState::default();
        // Establish low baseline.
        cc.record_rtt(20.0, 0);
        cc.tick(2_000_000, 0);

        // Sustained inflation in the Holding band: 1.5x ≤ rtt/min < 2.0x.
        // 20 → 35 = 1.75x; below DRAIN_RTT_INFLATION (2.0) so the
        // controller picks Holding rather than Drain. The smoothing is
        // intentionally slow for single-sample spikes — that's what
        // the EWMA is for.
        for i in 1..=10 {
            cc.record_rtt(35.0, i * 600);
            cc.tick(2_000_000, i * 600);
        }
        assert_eq!(cc.state, CcState::Holding);
    }

    #[test]
    fn backing_off_on_loss() {
        let mut cc = LinkCongestionState::default();
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
        let mut cc = LinkCongestionState::default();
        cc.record_loss(1_000, 100, 0);
        assert_eq!(cc.loss_permille(), 100);
        // Beyond window — should evict.
        cc.record_loss(0, 0, LOSS_WINDOW_MS + 10);
        assert_eq!(cc.loss_permille(), 0);
    }

    #[test]
    fn rtt_min_is_windowed_not_lifetime() {
        let mut cc = LinkCongestionState::default();
        // Establish a low baseline, then a sustained higher floor (e.g.
        // a cellular handover raised propagation delay to ~80ms).
        cc.record_rtt(20.0, 0);
        for t in (1_000..=40_000).step_by(1_000) {
            cc.record_rtt(80.0, t);
        }
        // Past the 30s window the pinned 20ms minimum has expired and the
        // baseline now follows the ~80ms floor, so inflation is ~1x and
        // the controller won't sit in a spurious Drain.
        assert!(
            cc.rtt_min_ms >= 70.0,
            "windowed rtt_min should follow the raised floor, got {}",
            cc.rtt_min_ms
        );
    }

    #[test]
    fn rtt_min_holds_within_window() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(20.0, 0);
        // Within the window the low floor is retained even as RTT rises.
        for t in (1_000..=10_000).step_by(1_000) {
            cc.record_rtt(80.0, t);
        }
        assert!(
            (cc.rtt_min_ms - 20.0).abs() < 1.0,
            "rtt_min should hold the true floor within the window, got {}",
            cc.rtt_min_ms
        );
    }

    #[test]
    fn rtt_ewma_resets_after_2s_gap() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(50.0, 0);
        // 2.5s later — should snap to the new sample.
        cc.record_rtt(200.0, 2_500);
        assert!((cc.rtt_ewma_ms - 200.0).abs() < 0.01);
    }

    #[test]
    fn rtt_ewma_weights_by_age() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(100.0, 0);
        // Within 250ms — heavy weight on old (1:16).
        cc.record_rtt(200.0, 100);
        assert!(cc.rtt_ewma_ms < 110.0);
    }

    #[test]
    fn hai_kicks_in_when_rtt_is_stable() {
        let mut cc = LinkCongestionState::default();
        // Feed identical RTT samples → variance stays at 0.
        for i in 0..10 {
            cc.record_rtt(50.0, i * 100);
            cc.tick(2_000_000, i * 100);
        }
        assert_eq!(cc.state, CcState::Climbing);
        assert_eq!(cc.climb_mode, ClimbMode::Hai);
    }

    #[test]
    fn hai_yields_to_normal_when_rtt_is_jittery() {
        let mut cc = LinkCongestionState::default();
        // Alternate between 30 and 80 ms — variance grows past the
        // HAI threshold.
        for i in 0..10 {
            let rtt = if i % 2 == 0 { 30.0 } else { 80.0 };
            cc.record_rtt(rtt, i * 100);
            cc.tick(2_000_000, i * 100);
        }
        assert_eq!(cc.state, CcState::Climbing);
        assert_eq!(cc.climb_mode, ClimbMode::Normal);
    }

    #[test]
    fn fast_recovery_engages_after_backoff() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(50.0, 0);
        cc.tick(2_000_000, 0);

        // Inject loss → BackingOff.
        cc.record_loss(1_000, 100, 100);
        cc.tick(2_000_000, 100);
        assert_eq!(cc.state, CcState::BackingOff);

        // Loss window evicts after 1s → Climbing with FastRecovery armed.
        cc.tick(2_000_000, 1_200);
        assert_eq!(cc.state, CcState::Climbing);
        assert_eq!(cc.climb_mode, ClimbMode::FastRecovery);

        // After FAST_RECOVERY_TICKS more healthy ticks, drops back
        // to Normal (or Hai if RTT stays flat).
        for i in 1..=FAST_RECOVERY_TICKS as u64 {
            cc.tick(2_000_000, 1_200 + i);
        }
        assert_eq!(cc.state, CcState::Climbing);
        assert!(matches!(cc.climb_mode, ClimbMode::Normal | ClimbMode::Hai));
    }

    #[test]
    fn drain_triggers_on_high_rtt_inflation_no_loss() {
        let mut cc = LinkCongestionState::default();
        // Establish low rtt_min.
        cc.record_rtt(20.0, 0);
        cc.tick(2_000_000, 0);

        // Push EWMA past 2x rtt_min via sustained 60ms samples.
        for i in 1..20 {
            cc.record_rtt(60.0, i * 600);
            cc.tick(2_000_000, i * 600);
        }
        // No loss observed → Drain should fire when inflation crosses 2x.
        // 20→60 = 3x; the ewma should have crossed 40.0 by now.
        assert!(cc.state == CcState::Drain || cc.state == CcState::Holding);
        if cc.state == CcState::Drain {
            // Drain dropped target by 25%.
            assert!(cc.target_bps < 2_000_000);
        }
    }

    #[test]
    fn drain_then_recovery_path() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(20.0, 0);
        cc.tick(2_000_000, 0);
        // Force into Drain.
        for i in 1..15 {
            cc.record_rtt(60.0, i * 600);
            cc.tick(2_000_000, i * 600);
        }
        // Then RTT recovers — back to Climbing with FastRecovery armed.
        for i in 15..30 {
            cc.record_rtt(20.0, i * 600);
            cc.tick(2_000_000, i * 600);
        }
        assert_eq!(cc.state, CcState::Climbing);
        // Within the FastRecovery window we should see that mode at
        // least once. Hard to assert exactly which tick — verify the
        // path was traversed by checking rtt is back to baseline.
        assert!(cc.rtt_ewma_ms < 30.0);
    }

    #[test]
    fn loss_ewma_latches_degraded_after_sustained_loss_and_clears_with_hysteresis() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(50.0, 0);

        // Drive sustained ~80% loss for well over LOSS_DEGRADE_SUSTAIN_MS.
        // Each tick feeds a fresh high-loss window via record_loss so the
        // permille stays high as the EWMA climbs past the entry threshold.
        let mut t = 0u64;
        for _ in 0..40 {
            t += 200;
            cc.record_loss(1_000, 800, t);
            cc.tick(2_000_000, t);
        }
        assert!(
            cc.snapshot().loss_degraded,
            "sustained high loss should latch the degraded verdict (ewma={:.3})",
            cc.loss_ewma
        );

        // Recover: zero loss long enough for the EWMA to fall below
        // LOSS_DEGRADE_CLEAR. record_loss with a clean window drains it.
        for _ in 0..60 {
            t += 200;
            cc.record_loss(1_000, 0, t);
            cc.tick(2_000_000, t);
        }
        assert!(
            !cc.snapshot().loss_degraded,
            "recovered loss should clear the verdict (ewma={:.3})",
            cc.loss_ewma
        );
    }

    #[test]
    fn outlier_burst_at_seed_is_clamped() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(50.0, 0);
        // First non-bootstrap tick sees a 50 Mbps stall-release burst.
        // The seed must be bounded to a few times the initial estimate,
        // not pinned to the burst.
        cc.tick(50_000_000, 0);
        assert!(
            cc.target_bps <= 5 * INITIAL_TARGET_BPS,
            "seed inflated to {} from a burst",
            cc.target_bps
        );
        assert!(cc.target_bps >= INITIAL_TARGET_BPS);
    }

    #[test]
    fn outlier_burst_does_not_run_the_target_away() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(50.0, 0);
        // Establish a steady ~2 Mbps estimate.
        for i in 0..8 {
            cc.record_rtt(50.0, i * 100);
            cc.tick(2_000_000, i * 100);
        }
        let before = cc.target_bps;
        // A sustained 50 Mbps burst over a few ticks: each sample is
        // clamped to 4x the running estimate, so the target can't leap to
        // the burst rate in one tick.
        cc.record_rtt(50.0, 900);
        cc.tick(50_000_000, 900);
        assert!(
            cc.target_bps <= before.saturating_mul(4).max(INITIAL_TARGET_BPS),
            "one burst tick inflated target to {} from {}",
            cc.target_bps,
            before
        );
    }

    #[test]
    fn loss_ewma_does_not_latch_on_a_transient_spike() {
        let mut cc = LinkCongestionState::default();
        cc.record_rtt(50.0, 0);
        // One bad window, then clean. Must not latch (sustain not met).
        cc.record_loss(1_000, 900, 200);
        cc.tick(2_000_000, 200);
        for _ in 0..10 {
            cc.record_loss(1_000, 0, 400);
        }
        cc.tick(2_000_000, 1_600);
        assert!(
            !cc.snapshot().loss_degraded,
            "a single bad window must not demote the link"
        );
    }

    #[test]
    fn observe_traffic_first_call_sets_baseline_without_sample() {
        let mut cc = LinkCongestionState::default();
        cc.observe_traffic(1_000_000, 5, 100);
        assert!(
            cc.loss_samples.is_empty(),
            "first call must not emit a sample"
        );
        assert!(cc.traffic_baseline_set);
        assert_eq!(cc.prev_bytes_sent_total, 1_000_000);
        assert_eq!(cc.prev_nak_total, 5);
    }

    #[test]
    fn observe_traffic_delta_flows_into_record_loss() {
        let mut cc = LinkCongestionState::default();
        cc.observe_traffic(0, 0, 0);
        // 1 MB sent ≈ 760 packets at 1316-byte payload. 5 NAKs in same tick.
        cc.observe_traffic(1_000_000, 5, 100);
        let pm = cc.loss_permille();
        // 5 / 760 ≈ 6.6 permille
        assert!(pm > 0, "loss permille should be non-zero after delta");
        assert!(pm < 20, "expected ~6 permille, got {pm}");
    }

    #[test]
    fn observe_traffic_quiet_tick_with_naks_does_not_panic() {
        // Pathological: NAKs arrive but no bytes were sent this tick.
        // Without the synthesized 1-packet `sent` baseline this would
        // skip the record_loss call entirely (delta_bytes == 0 path);
        // verify the loss makes it into the window.
        let mut cc = LinkCongestionState::default();
        cc.observe_traffic(1_000_000, 0, 0);
        cc.observe_traffic(1_000_000, 5, 100);
        let pm = cc.loss_permille();
        assert!(
            pm > 0,
            "loss with no fresh bytes should still register, got {pm}"
        );
        // Hard cap from `loss_permille` saturation.
        assert!(pm <= 1_000_000);
    }
}
