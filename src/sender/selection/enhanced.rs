//! Enhanced connection selection algorithm
//!
//! This module implements the enhanced SRTLA connection selection with:
//! - Quality-aware scoring based on NAK history
//! - RTT-aware bonuses for low-latency connections
//! - Minimal hysteresis to prevent flip-flopping (2%)
//! - Optional smart exploration of alternative connections
//!
//! The enhanced mode provides better connection quality awareness while
//! maintaining natural load distribution across all connections.

use tracing::debug;

use super::MIN_SWITCH_INTERVAL_MS;
use super::exploration::should_explore_now;
use super::quality::calculate_quality_multiplier;
use crate::connection::SrtlaConnection;

/// Switching hysteresis: require new connection to be significantly better
/// REDUCED to 2% to allow better load distribution across multiple connections
/// Original 15% was preventing traffic from spreading across all uplinks
const SWITCH_THRESHOLD: f64 = 1.02; // New connection must be 2% better

/// Select best connection using enhanced algorithm with quality awareness
///
/// Returns the index of the connection with the best quality-adjusted score.
/// Implements time-based switch dampening to prevent rapid thrashing.
///
/// IMPORTANT: This function is called for EACH incoming SRT packet. The returned
/// connection index determines where that packet (and subsequent packets) will be routed.
/// Time-based dampening prevents changing the routing decision too frequently, ensuring
/// all packets continue flowing through the same connection during the cooldown period.
/// This is NOT a per-packet round-robin - it's a per-packet "best connection" selection
/// with dampening to prevent rapid switching under bursty network conditions.
///
/// # Arguments
/// * `conns` - Slice of available connections
/// * `last_idx` - Previously selected connection index (for hysteresis)
/// * `last_switch_time_ms` - Timestamp of last connection switch
/// * `current_time_ms` - Current timestamp in milliseconds
/// * `enable_quality` - Whether to apply quality scoring
/// * `enable_explore` - Whether to enable smart exploration
pub fn select_connection(
    conns: &[SrtlaConnection],
    last_idx: Option<usize>,
    last_switch_time_ms: u64,
    current_time_ms: u64,
    enable_quality: bool,
    enable_explore: bool,
) -> Option<usize> {
    // Score connections by base score; apply quality multiplier if enabled
    let mut best_idx: Option<usize> = None;
    let mut second_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;
    let mut second_score: f64 = -1.0;
    let mut current_score: Option<f64> = None;

    for (i, c) in conns.iter().enumerate() {
        if c.is_timed_out() {
            continue;
        }
        let base = c.get_score() as f64;
        let score = if !enable_quality {
            base
        } else {
            let quality_mult = calculate_quality_multiplier(c, current_time_ms);
            let final_score = base * quality_mult;

            // Log quality issues and recoveries for debugging
            if quality_mult < 0.8 {
                debug!(
                    "{} quality degraded: {:.2} (NAKs: {}, last: {}ms ago, burst: {}) base: {} → \
                     final: {}",
                    c.label,
                    quality_mult,
                    c.total_nak_count(),
                    c.time_since_last_nak_ms().unwrap_or(0),
                    c.nak_burst_count(),
                    base as i32,
                    final_score as i32
                );
            } else if quality_mult < 1.0 && c.nak_burst_count() > 0 {
                debug!(
                    "{} quality recovering: {:.2} (burst: {})",
                    c.label,
                    quality_mult,
                    c.nak_burst_count()
                );
            }

            final_score
        };

        // Track current connection's score for hysteresis
        if Some(i) == last_idx {
            current_score = Some(score);
        }

        if score > best_score {
            second_score = best_score;
            second_idx = best_idx;
            best_score = score;
            best_idx = Some(i);
        } else if score > second_score {
            second_score = score;
            second_idx = Some(i);
        }
    }

    // Time-based switch dampening: prevent rapid thrashing under bursty scores
    // Check if we're within the minimum switch interval
    let time_since_last_switch_ms = current_time_ms.saturating_sub(last_switch_time_ms);
    let in_switch_cooldown = time_since_last_switch_ms < MIN_SWITCH_INTERVAL_MS;

    if let Some(last) = last_idx {
        // If proposing a different connection
        if best_idx != Some(last) {
            // Check if last connection is still valid
            let last_still_valid =
                last < conns.len() && !conns[last].is_timed_out() && conns[last].connected;

            // If in cooldown period and last connection is still valid, keep it
            if in_switch_cooldown && last_still_valid {
                debug!(
                    "Switch dampening: staying with current connection (cooldown: {}ms remaining)",
                    MIN_SWITCH_INTERVAL_MS.saturating_sub(time_since_last_switch_ms)
                );
                return Some(last);
            }

            // Apply score-based hysteresis if not in cooldown
            // If current connection is still valid and new best isn't significantly better
            if let Some(current) = current_score {
                if best_score < current * SWITCH_THRESHOLD {
                    // Only log occasionally to reduce spam
                    if current_time_ms % 1000 < 10 {
                        debug!(
                            "Score hysteresis: staying with current connection (current: {:.1}, \
                             best: {:.1}, threshold: {:.1})",
                            current,
                            best_score,
                            current * SWITCH_THRESHOLD
                        );
                    }
                    return Some(last);
                }
            }
        }
    }

    // Apply exploration if enabled (but respect cooldown to avoid rapid switching)
    let explore_now = if enable_explore && !in_switch_cooldown {
        should_explore_now(conns, best_idx, second_idx)
    } else {
        false
    };

    if explore_now {
        // Exploration wants to try second-best, but only if different from current
        if let (Some(second), Some(last)) = (second_idx, last_idx) {
            if second != last {
                debug!("Exploration: trying second-best connection");
                return second_idx.or(best_idx);
            }
        }
        // If second is same as current, just use best
        best_idx
    } else {
        best_idx
    }
}

// Tests are in src/tests/sender_tests.rs
