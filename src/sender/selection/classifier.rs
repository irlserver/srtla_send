//! Weak-link classifier (shadow mode).
//!
//! Computes a per-connection `weak: bool` flag using a three-tier delay
//! cascade and entering/leaving thresholds with hysteresis. **Currently
//! shadow mode only** — the result is exposed via stats telemetry but
//! does not influence selection. Wire into Enhanced selection only after
//! a soak window confirms the classifier matches operator intuition.
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
//!    - its RTT exceeds the chosen tier (high latency), or
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

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WeakReason {
    /// Link passed all checks. Not weak.
    Healthy,
    /// Link's RTT exceeds the chosen delay tier.
    HighRtt,
    /// Link is connected but delivered no traffic in the window.
    NoTraffic,
    /// Link's throughput share is below the entering threshold (or, if
    /// previously weak, below the leaving threshold).
    LowShare,
    /// Total throughput below the classification floor — every link
    /// reported as not-weak.
    Bypassed,
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
/// hysteresis pass can use the leaving threshold for those.
#[derive(Default)]
pub struct WeakLinkFilter {
    prev_weak: HashMap<u64, bool>,
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

            let (weak, reason) = if rtt_ms > selected_delay {
                (true, WeakReason::HighRtt)
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
}
