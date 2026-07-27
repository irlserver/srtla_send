//! Weak-link classifier.
//!
//! Computes a per-connection `weak: bool` flag using a three-tier delay
//! cascade and entering/leaving thresholds with hysteresis. The result is
//! consumed by Enhanced selection as an admission gate (a weak link's
//! routing score is crushed but the link stays rankable).
//!
//! ## Algorithm
//!
//! 1. Estimate a per-stream max delay budget. We don't have a peer-side
//!    estimate, so derive it locally as `max(longest_rtt * 3, 500ms)`
//!    capped at `5000ms`.
//! 2. Three delay tiers: `best = 40%`, `safe = 50%`, `max = 60%` of the
//!    estimate, capped at 2.5s / 2.5s / 5s.
//! 3. Bucket each link's recent throughput by which tier its RTT meets.
//!    Pick the tightest tier where >=85% of throughput still fits, with
//!    a 50%/25% cascade fallback for degraded conditions.
//! 4. Mark a link weak if either:
//!    - its RTT exceeds the chosen tier (high latency) or a standing
//!      queue is forming, sustained for `WEAK_SUSTAIN_TICKS` consecutive
//!      housekeeping ticks. Both signals flip on a single evaluation, so
//!      the streak latch filters one-tick (~1s) blips before the gate
//!      demotes routing weight, or
//!    - its share of total throughput falls below the entering
//!      threshold. Once weak, the link stays weak until its share rises
//!      above the (much higher) leaving threshold.
//!
//! ## Tuning
//!
//! Numbers below are starting points picked to be conservative. Real-
//! world soak data may suggest retuning.
//!
//! - **Tier ratios 40/50/60% with 2.5/2.5/5s caps**: physical
//!   proportions of an estimated budget.
//! - **Bandwidth-share cutoffs 85/50/25%**: same.
//! - **Entering threshold = 0.25 / N of fair share**: a link delivering
//!   less than a quarter of its expected share is suspect.
//! - **Leaving threshold = 0.75 / N of fair share**: to clear weak
//!   status, a link must approach fair share. **3x hysteresis ratio**
//!   between enter and leave keeps marginal links from flapping.

use std::collections::HashMap;

use crate::connection::SrtlaConnection;

/// Cap on `target_best_delay_ms` and `target_safe_delay_ms`.
const TARGET_BEST_SAFE_CAP_MS: u32 = 2500;
/// Cap on `target_max_delay_ms`.
const TARGET_MAX_CAP_MS: u32 = 5000;

/// Estimate-from-RTT multiplier when no peer-side budget is available.
const RTT_TO_DELAY_BUDGET_MULT: f64 = 3.0;
/// Floor for the derived budget — prevents pathological ramp on tiny RTTs.
const MIN_BUDGET_MS: u32 = 500;
/// Hard upper bound on the derived budget.
const MAX_BUDGET_MS: u32 = 5000;

/// Bandwidth-share cutoffs for tier selection.
const SHARE_85_PERMILLE: u64 = 850;
const SHARE_50_PERMILLE: u64 = 500;
const SHARE_25_PERMILLE: u64 = 250;

/// Entering / leaving thresholds expressed as a permille of fair share.
/// `enter_share = (1000 / n_links) * 0.25`; `leave_share = ... * 0.75`.
/// 3× hysteresis ratio.
const ENTER_FAIR_SHARE_NUMERATOR: u64 = 250;
const LEAVE_FAIR_SHARE_NUMERATOR: u64 = 750;

/// Below this total throughput, classification is bypassed and every
/// connected link is treated as not-weak (we don't have enough signal).
const MIN_TOTAL_BPS_FOR_CLASSIFICATION: f64 = 100_000.0;

/// Consecutive housekeeping ticks a delay signal (`HighRtt` /
/// `QueueBuilding`) must persist before the link is marked weak. The
/// housekeeping loop runs once per second, so 2 ticks ≈ 2s: long enough
/// to filter a single one-second RTT/queue blip, short enough to demote a
/// genuinely congesting link well before it hurts. A real collapse holds
/// the signal for many seconds, so reaction speed is unaffected. Do not
/// raise above 3.
const WEAK_SUSTAIN_TICKS: u32 = 2;

/// After a link has been continuously share-weak (LowShare/NoTraffic) for
/// this many housekeeping ticks (~1Hz, so ~15s), force a probation re-test.
/// Delay- and loss-driven weakness are exempt: those self-clear from live
/// RTT/loss without needing traffic, so they can't latch.
const PROBATION_INTERVAL_TICKS: u32 = 15;

/// Length of the probation re-test window in ticks (~3s). The link is
/// treated as not-weak for this long so selection routes it real traffic and
/// it can re-prove its throughput share. A link that is genuinely bad still
/// stays gated by the independent `loss_degraded` / delay gates even inside
/// this window, so probation only ever re-tests marginal-but-usable links.
///
/// NOTE: both probation constants are starting points. Validate the window
/// length in the network-sim harness before treating them as final: too
/// short and a recovered link can't accrue enough share to clear the
/// entering threshold; too long and a genuinely starved link draws traffic
/// it can't use.
const PROBATION_WINDOW_TICKS: u32 = 3;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WeakReason {
    /// Link passed all checks. Not weak.
    Healthy,
    /// Link's RTT exceeds the chosen delay tier.
    HighRtt,
    /// Link's RTT is still within tier but a standing queue is forming
    /// (jitter-immune delay gradient). Early warning before HighRtt.
    QueueBuilding,
    /// Link is connected but delivered no traffic in the window.
    NoTraffic,
    /// Link's throughput share is below the entering threshold (or, if
    /// previously weak, below the leaving threshold).
    LowShare,
    /// Total throughput below the classification floor — every link
    /// reported as not-weak.
    Bypassed,
}

impl WeakReason {
    /// Whether this verdict is about *delay* — the link is late, not merely
    /// under-used. Selection treats the two differently: a late link must not
    /// carry unique payload at all (its packets are what stalls the receiver's
    /// reorder buffer), while an under-used one is kept on a small share of
    /// real traffic, which is how it earns back the throughput share that
    /// clears the verdict.
    #[inline(always)]
    pub fn is_delay(self) -> bool {
        matches!(self, WeakReason::HighRtt | WeakReason::QueueBuilding)
    }
}

#[derive(Clone, Debug)]
pub struct LinkClassification {
    pub conn_id: u64,
    pub weak: bool,
    pub reason: WeakReason,
    /// Throughput share in permille of total (0..=1000).
    pub share_permille: u32,
    /// Threshold the share was checked against (permille).
    pub threshold_permille: u32,
}

#[derive(Clone, Debug)]
pub struct ClassificationResult {
    /// Delay tier the cascade chose this run (ms). Zero when classification was bypassed.
    pub selected_delay_ms: u32,
    /// Estimated max delay budget the tiers were derived from.
    pub estimated_max_delay_ms: u32,
    pub per_link: Vec<LinkClassification>,
}

/// Stateful filter: tracks `previously_weak` per connection so the
/// hysteresis pass can use the leaving threshold for those, and the
/// per-connection consecutive-tick streak of an active delay signal so
/// `HighRtt`/`QueueBuilding` only mark weak once sustained.
#[derive(Default)]
pub struct WeakLinkFilter {
    prev_weak: HashMap<u64, bool>,
    delay_weak_streak: HashMap<u64, u32>,
    /// Consecutive ticks a link has been share-weak (LowShare/NoTraffic),
    /// used to trigger a probation re-test once it exceeds
    /// `PROBATION_INTERVAL_TICKS`.
    weak_streak: HashMap<u64, u32>,
    /// Remaining forced not-weak ticks for a link currently inside a
    /// probation re-test window.
    probation_ticks: HashMap<u64, u32>,
}

impl WeakLinkFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn classify(&mut self, conns: &[SrtlaConnection]) -> ClassificationResult {
        let mut per_link: Vec<LinkClassification> = Vec::with_capacity(conns.len());

        // First pass: gather signals from connected links.
        let mut total_bps: f64 = 0.0;
        let mut longest_rtt_ms: u32 = 0;
        let mut connected_count: usize = 0;

        for conn in conns {
            if !conn.connected {
                continue;
            }
            connected_count += 1;
            total_bps += conn.bitrate.current_bitrate_bps.max(0.0);
            let rtt_ms = conn.get_smooth_rtt_ms() as u32;
            if rtt_ms > longest_rtt_ms {
                longest_rtt_ms = rtt_ms;
            }
        }

        // Below the floor — bypass classification, mark everything healthy.
        if total_bps < MIN_TOTAL_BPS_FOR_CLASSIFICATION || connected_count == 0 {
            for conn in conns {
                per_link.push(LinkClassification {
                    conn_id: conn.conn_id,
                    weak: false,
                    reason: WeakReason::Bypassed,
                    share_permille: 0,
                    threshold_permille: 0,
                });
            }
            // Reset hysteresis history so we don't carry stale weak flags
            // across an idle period.
            self.prev_weak.clear();
            self.delay_weak_streak.clear();
            self.weak_streak.clear();
            self.probation_ticks.clear();
            return ClassificationResult {
                selected_delay_ms: 0,
                estimated_max_delay_ms: 0,
                per_link,
            };
        }

        let estimated_max_delay_ms = derive_max_delay_budget(longest_rtt_ms);
        let target_best = target_best_delay_ms(estimated_max_delay_ms);
        let target_safe = target_safe_delay_ms(estimated_max_delay_ms);
        let target_max = target_max_delay_ms(estimated_max_delay_ms);

        // Second pass: bucket throughput by tier.
        let mut bytes_per_sec_best: f64 = 0.0;
        let mut bytes_per_sec_safe: f64 = 0.0;
        let mut bytes_per_sec_max: f64 = 0.0;
        for conn in conns {
            if !conn.connected {
                continue;
            }
            let bps = conn.bitrate.current_bitrate_bps.max(0.0);
            let rtt_ms = conn.get_smooth_rtt_ms() as u32;
            if rtt_ms <= target_best {
                bytes_per_sec_best += bps;
            }
            if rtt_ms <= target_safe {
                bytes_per_sec_safe += bps;
            }
            if rtt_ms <= target_max {
                bytes_per_sec_max += bps;
            }
        }

        let selected_delay = pick_tier(
            total_bps,
            bytes_per_sec_best,
            bytes_per_sec_safe,
            bytes_per_sec_max,
            target_best,
            target_safe,
            target_max,
        );

        // Third pass: classify each link.
        let n_connected = connected_count as u64;
        let enter_threshold_permille = (ENTER_FAIR_SHARE_NUMERATOR / n_connected) as u32;
        let leave_threshold_permille = (LEAVE_FAIR_SHARE_NUMERATOR / n_connected) as u32;
        let mut next_prev_weak: HashMap<u64, bool> = HashMap::with_capacity(conns.len());
        let mut next_delay_streak: HashMap<u64, u32> = HashMap::with_capacity(conns.len());
        let mut next_weak_streak: HashMap<u64, u32> = HashMap::with_capacity(conns.len());
        let mut next_probation: HashMap<u64, u32> = HashMap::with_capacity(conns.len());

        for conn in conns {
            if !conn.connected {
                per_link.push(LinkClassification {
                    conn_id: conn.conn_id,
                    weak: false,
                    reason: WeakReason::Healthy,
                    share_permille: 0,
                    threshold_permille: 0,
                });
                continue;
            }

            let rtt_ms = conn.get_smooth_rtt_ms() as u32;
            let bps = conn.bitrate.current_bitrate_bps.max(0.0);
            let share_permille = if total_bps > 0.0 {
                ((bps * 1000.0) / total_bps).clamp(0.0, 1000.0) as u32
            } else {
                0
            };

            let was_weak = self.prev_weak.get(&conn.conn_id).copied().unwrap_or(false);
            let threshold = if was_weak {
                leave_threshold_permille
            } else {
                enter_threshold_permille
            };

            // Delay signals (RTT over tier, or a forming queue) flip on a
            // single evaluation, so gate them behind a consecutive-tick
            // streak. Count up while a delay signal is active, reset to 0
            // the moment it clears; only mark weak once the streak reaches
            // WEAK_SUSTAIN_TICKS, filtering one-tick blips.
            let delay_signal = if rtt_ms > selected_delay {
                Some(WeakReason::HighRtt)
            } else if conn.queue_building_suspected() {
                Some(WeakReason::QueueBuilding)
            } else {
                None
            };
            let delay_streak = if delay_signal.is_some() {
                self.delay_weak_streak
                    .get(&conn.conn_id)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1)
            } else {
                0
            };
            next_delay_streak.insert(conn.conn_id, delay_streak);
            let delay_weak = delay_streak >= WEAK_SUSTAIN_TICKS;

            let (weak, reason) = if delay_weak {
                // Sustained: keep it rankable (the gate crushes score but
                // never removes), so this only de-prioritises.
                (true, delay_signal.unwrap())
            } else if bps == 0.0 {
                (true, WeakReason::NoTraffic)
            } else if was_weak && share_permille < leave_threshold_permille {
                // Stays weak until share clears the leaving threshold.
                (true, WeakReason::LowShare)
            } else if !was_weak && share_permille < enter_threshold_permille {
                (true, WeakReason::LowShare)
            } else {
                (false, WeakReason::Healthy)
            };

            // Probation re-test — breaks the share-starvation latch (R1). A
            // link gated for low share earns a crushed routing score, gets
            // ~no traffic, so its share stays low and it stays gated: a
            // self-sustaining lock the GATED_LINK_PENALTY trickle can't escape.
            // After PROBATION_INTERVAL_TICKS
            // continuously share-weak, force a PROBATION_WINDOW_TICKS window
            // treating the link as not-weak, so selection routes it real
            // traffic and it can re-prove its share. Emitting not-weak clears
            // prev_weak across the window, so the post-window judgement uses
            // the (lower) entering threshold and a recovered link can actually
            // win the re-test.
            //
            // A live delay verdict cancels the window outright. Probation is
            // only ever armed by *share* weakness — a link starved of traffic,
            // which real traffic is the only way to re-test — but the window
            // used to override whatever the current tick computed, including a
            // sustained HighRtt/QueueBuilding verdict that appeared during it.
            // That handed a link we can see is late a full share of unique
            // payload for the rest of the window, which is the receiver stall
            // the delay gate exists to prevent, on a timer. If the re-test is
            // answered, and answered badly, it is over: the link waits out
            // another share-weak streak before the next one. `loss_degraded`
            // never routed through here at all — it gates independently — so
            // it was, and remains, unaffected.
            let share_weak = weak && matches!(reason, WeakReason::LowShare | WeakReason::NoTraffic);
            let mut probation = self
                .probation_ticks
                .get(&conn.conn_id)
                .copied()
                .unwrap_or(0);
            let mut streak = self.weak_streak.get(&conn.conn_id).copied().unwrap_or(0);
            let (weak, reason) = if probation > 0 && delay_weak {
                probation = 0;
                streak = 0;
                (weak, reason)
            } else if probation > 0 {
                probation -= 1;
                streak = 0;
                (false, WeakReason::Healthy)
            } else if share_weak {
                streak = streak.saturating_add(1);
                if streak >= PROBATION_INTERVAL_TICKS {
                    // Arm the window; this trigger tick stays gated, the next
                    // PROBATION_WINDOW_TICKS ticks are forced not-weak.
                    streak = 0;
                    probation = PROBATION_WINDOW_TICKS;
                }
                (weak, reason)
            } else {
                streak = 0;
                (weak, reason)
            };
            next_weak_streak.insert(conn.conn_id, streak);
            next_probation.insert(conn.conn_id, probation);

            next_prev_weak.insert(conn.conn_id, weak);
            per_link.push(LinkClassification {
                conn_id: conn.conn_id,
                weak,
                reason,
                share_permille,
                threshold_permille: threshold,
            });
            // Suppress unused-variable warning when consumers ignore rtt_ms.
            let _ = rtt_ms;
        }

        self.prev_weak = next_prev_weak;
        self.delay_weak_streak = next_delay_streak;
        self.weak_streak = next_weak_streak;
        self.probation_ticks = next_probation;
        ClassificationResult {
            selected_delay_ms: selected_delay,
            estimated_max_delay_ms,
            per_link,
        }
    }
}

fn derive_max_delay_budget(longest_rtt_ms: u32) -> u32 {
    let raw = (longest_rtt_ms as f64 * RTT_TO_DELAY_BUDGET_MULT) as u32;
    raw.clamp(MIN_BUDGET_MS, MAX_BUDGET_MS)
}

fn target_best_delay_ms(est_ms: u32) -> u32 {
    ((est_ms as u64 * 40) / 100).min(TARGET_BEST_SAFE_CAP_MS as u64) as u32
}

fn target_safe_delay_ms(est_ms: u32) -> u32 {
    ((est_ms as u64 * 50) / 100).min(TARGET_BEST_SAFE_CAP_MS as u64) as u32
}

fn target_max_delay_ms(est_ms: u32) -> u32 {
    ((est_ms as u64 * 60) / 100).min(TARGET_MAX_CAP_MS as u64) as u32
}

fn pick_tier(
    total_bps: f64,
    best_bps: f64,
    safe_bps: f64,
    max_bps: f64,
    best_delay: u32,
    safe_delay: u32,
    max_delay: u32,
) -> u32 {
    // Permille shares of total in each bucket.
    let best_pm = ((best_bps * 1000.0) / total_bps) as u64;
    let safe_pm = ((safe_bps * 1000.0) / total_bps) as u64;
    let max_pm = ((max_bps * 1000.0) / total_bps) as u64;

    if best_pm > SHARE_85_PERMILLE {
        return best_delay;
    }
    if safe_pm > SHARE_85_PERMILLE {
        return safe_delay;
    }
    if max_pm > SHARE_85_PERMILLE {
        // Degraded — fall through to 50%/25% cascade.
        if best_pm > SHARE_50_PERMILLE {
            return best_delay;
        }
        if safe_pm > SHARE_50_PERMILLE {
            return safe_delay;
        }
        if max_pm > SHARE_50_PERMILLE {
            return max_delay;
        }
        if best_pm > SHARE_25_PERMILLE {
            return best_delay;
        }
        if safe_pm > SHARE_25_PERMILLE {
            return safe_delay;
        }
        return max_delay;
    }
    max_delay
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_tier_math() {
        assert_eq!(target_best_delay_ms(1000), 400);
        assert_eq!(target_safe_delay_ms(1000), 500);
        assert_eq!(target_max_delay_ms(1000), 600);

        // Caps
        assert_eq!(target_best_delay_ms(10_000), TARGET_BEST_SAFE_CAP_MS);
        assert_eq!(target_safe_delay_ms(10_000), TARGET_BEST_SAFE_CAP_MS);
        assert_eq!(target_max_delay_ms(10_000), TARGET_MAX_CAP_MS);
    }

    #[test]
    fn budget_floor_and_ceiling() {
        assert_eq!(derive_max_delay_budget(50), MIN_BUDGET_MS);
        assert_eq!(derive_max_delay_budget(2000), MAX_BUDGET_MS);
        assert_eq!(derive_max_delay_budget(500), 1500);
    }

    #[test]
    fn pick_tier_picks_best_when_85pct_fits() {
        let tier = pick_tier(1000.0, 900.0, 950.0, 1000.0, 100, 200, 300);
        assert_eq!(tier, 100);
    }

    #[test]
    fn pick_tier_falls_back_to_safe() {
        let tier = pick_tier(1000.0, 100.0, 900.0, 1000.0, 100, 200, 300);
        assert_eq!(tier, 200);
    }

    #[test]
    fn pick_tier_falls_back_to_max() {
        let tier = pick_tier(1000.0, 0.0, 0.0, 100.0, 100, 200, 300);
        assert_eq!(tier, 300);
    }

    #[test]
    fn empty_classification_returns_bypassed() {
        let mut filter = WeakLinkFilter::new();
        let result = filter.classify(&[]);
        assert_eq!(result.selected_delay_ms, 0);
        assert!(result.per_link.is_empty());
    }

    // --- Probation re-test ---

    fn set_rtt(conn: &mut SrtlaConnection, rtt_ms: f64) {
        for _ in 0..16 {
            conn.rtt.kalman_rtt.update(rtt_ms);
        }
    }

    /// One healthy link carrying the stream plus one starved link. The starved
    /// link's share sits far below the entering threshold, so it is share-weak
    /// every tick and eventually earns a probation re-test.
    fn starved_pair() -> smallvec::SmallVec<SrtlaConnection, 4> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(crate::test_helpers::create_test_connections(2));
        conns[0].bitrate.current_bitrate_bps = 1_000_000.0;
        set_rtt(&mut conns[0], 50.0);
        conns[1].bitrate.current_bitrate_bps = 10_000.0;
        set_rtt(&mut conns[1], 50.0);
        conns
    }

    fn verdict(result: &ClassificationResult, conn_id: u64) -> (bool, WeakReason) {
        let e = result
            .per_link
            .iter()
            .find(|e| e.conn_id == conn_id)
            .expect("link classified");
        (e.weak, e.reason)
    }

    #[test]
    fn probation_re_tests_a_share_starved_link() {
        let conns = starved_pair();
        let id = conns[1].conn_id;
        let mut filter = WeakLinkFilter::new();

        // Share-weak every tick, and stays gated right up to the trigger tick.
        for tick in 0..PROBATION_INTERVAL_TICKS {
            let r = filter.classify(&conns);
            let (weak, reason) = verdict(&r, id);
            assert!(weak, "tick {tick}: starved link should be weak");
            assert_eq!(reason, WeakReason::LowShare);
        }

        // Window opens: real traffic is the only way to re-prove share.
        let (weak, reason) = verdict(&filter.classify(&conns), id);
        assert!(!weak, "probation must re-test the starved link");
        assert_eq!(reason, WeakReason::Healthy);
    }

    #[test]
    fn a_delay_verdict_cancels_the_probation_window() {
        // The hole this closes: probation used to override whatever the tick
        // computed, so a link that went late *during* its re-test kept a full
        // share of unique payload until the window ran out — the receiver
        // stall the delay gate exists to prevent, on a 15s timer.
        let mut conns = starved_pair();
        let id = conns[1].conn_id;
        let mut filter = WeakLinkFilter::new();

        for _ in 0..PROBATION_INTERVAL_TICKS {
            filter.classify(&conns);
        }
        // First window tick: still fast, so the re-test proceeds.
        let (weak, _) = verdict(&filter.classify(&conns), id);
        assert!(!weak, "precondition: the window opened");

        // Loaded at last, the link turns out to be badly late. A delay verdict
        // needs WEAK_SUSTAIN_TICKS consecutive samples to latch.
        set_rtt(&mut conns[1], 3000.0);
        let (weak, _) = verdict(&filter.classify(&conns), id);
        assert!(!weak, "one late sample is still just a blip");

        let (weak, reason) = verdict(&filter.classify(&conns), id);
        assert!(weak, "a sustained delay verdict must end the re-test");
        assert_eq!(reason, WeakReason::HighRtt);

        // ...and it is cancelled, not merely suspended: the remaining window
        // ticks must not resume handing the link unique payload.
        for tick in 0..PROBATION_WINDOW_TICKS {
            let (weak, reason) = verdict(&filter.classify(&conns), id);
            assert!(weak, "tick {tick} after cancellation must stay gated");
            assert_eq!(reason, WeakReason::HighRtt);
        }
    }

    #[test]
    fn a_link_that_is_late_never_earns_a_probation_window() {
        // Delay weakness must not arm probation in the first place: it clears
        // from live RTT, which keepalive echoes and probe ACKs keep supplying
        // even while the link is held out of the rotation.
        let mut conns = starved_pair();
        let id = conns[1].conn_id;
        set_rtt(&mut conns[1], 3000.0);
        let mut filter = WeakLinkFilter::new();

        for tick in 0..(PROBATION_INTERVAL_TICKS * 2) {
            let (weak, reason) = verdict(&filter.classify(&conns), id);
            assert!(weak, "tick {tick}: a late link stays weak");
            if tick >= WEAK_SUSTAIN_TICKS {
                assert_eq!(
                    reason,
                    WeakReason::HighRtt,
                    "tick {tick}: and stays late, never re-tested"
                );
            }
        }
    }
}
