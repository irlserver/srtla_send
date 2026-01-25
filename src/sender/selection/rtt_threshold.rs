//! RTT-threshold connection selection algorithm
//!
//! Groups links into "fast" and "slow" based on RTT, preferring fast links
//! to reduce packet reordering at the receiver.
//!
//! Algorithm:
//! 1. Find minimum RTT among eligible links
//! 2. Mark links as "fast" if: rtt <= min_rtt + delta
//! 3. Select link with best quality-adjusted capacity among fast links
//! 4. Fallback to any eligible link if no fast links have capacity

use tracing::debug;

use super::MIN_SWITCH_INTERVAL_MS;
use crate::connection::SrtlaConnection;

/// Select best connection using RTT-threshold algorithm
///
/// Prefers low-RTT links to reduce packet reordering, while still considering
/// capacity and quality within the "fast" link group.
///
/// # Arguments
/// * `conns` - Mutable slice of available connections (for quality cache updates)
/// * `last_idx` - Previously selected connection index (for dampening)
/// * `last_switch_time_ms` - Timestamp of last connection switch
/// * `current_time_ms` - Current timestamp in milliseconds
/// * `rtt_delta_ms` - RTT threshold above minimum to be considered "fast"
/// * `enable_quality` - Whether to apply quality scoring
#[inline(always)]
pub fn select_connection(
    conns: &mut [SrtlaConnection],
    last_idx: Option<usize>,
    last_switch_time_ms: u64,
    current_time_ms: u64,
    rtt_delta_ms: u32,
    enable_quality: bool,
) -> Option<usize> {
    // Phase 1: Find minimum RTT among eligible links
    let mut min_rtt = f64::MAX;
    for c in conns.iter() {
        if c.is_timed_out() || !c.connected {
            continue;
        }
        let base_score = c.get_score();
        if base_score <= 0 {
            continue;
        }
        let rtt = c.get_smooth_rtt_ms();
        // Only consider links with valid RTT measurements
        if rtt > 0.0 && rtt < min_rtt {
            min_rtt = rtt;
        }
    }

    // If no valid RTT data, treat all links as fast
    let rtt_threshold = if min_rtt == f64::MAX {
        f64::MAX
    } else {
        min_rtt + f64::from(rtt_delta_ms)
    };

    // Phase 2: Select best among fast links
    let mut best_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;

    for (i, c) in conns.iter_mut().enumerate() {
        if c.is_timed_out() || !c.connected {
            continue;
        }
        let base_score = c.get_score();
        if base_score <= 0 {
            continue;
        }

        let rtt = c.get_smooth_rtt_ms();
        // A link is "fast" if:
        // - No RTT data (rtt <= 0), or
        // - RTT is within threshold of minimum
        let is_fast = rtt <= 0.0 || rtt <= rtt_threshold;

        if is_fast {
            let score = if enable_quality {
                let quality = c.get_cached_quality_multiplier(current_time_ms);
                (base_score as f64) * quality
            } else {
                base_score as f64
            };

            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
    }

    // Phase 3: Fallback to any eligible link if no fast links have capacity
    if best_idx.is_none() {
        debug!(
            "RTT-threshold: no fast links available (threshold: {:.0}ms), falling back",
            rtt_threshold
        );
        for (i, c) in conns.iter_mut().enumerate() {
            if c.is_timed_out() || !c.connected {
                continue;
            }
            let base_score = c.get_score();
            if base_score <= 0 {
                continue;
            }
            let score = if enable_quality {
                let quality = c.get_cached_quality_multiplier(current_time_ms);
                (base_score as f64) * quality
            } else {
                base_score as f64
            };

            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
    }

    // Phase 4: Time-based dampening (prevent rapid thrashing)
    let time_since_last_switch = current_time_ms.saturating_sub(last_switch_time_ms);
    let in_cooldown = time_since_last_switch < MIN_SWITCH_INTERVAL_MS;

    if let Some(last) = last_idx {
        if best_idx != Some(last) && in_cooldown {
            // Check if last connection is still valid
            let last_valid =
                last < conns.len() && !conns[last].is_timed_out() && conns[last].connected;
            if last_valid && conns[last].get_score() > 0 {
                return Some(last);
            }
        }
    }

    best_idx
}
