use tracing::debug;

use crate::connection::SrtlaConnection;
use crate::utils::{elapsed_ms, now_ms};

/// Minimum time in milliseconds between connection switches
/// Prevents rapid thrashing even when scores are identical
/// Set to 0 to disable and allow natural per-packet load distribution

pub fn calculate_quality_multiplier(conn: &SrtlaConnection) -> f64 {
    // Startup grace period: first 30 seconds after connection establishment
    // During this time, use simple scoring like original C version to go live fast
    // This prevents early NAKs from permanently degrading connections
    let connection_age_ms = now_ms().saturating_sub(conn.connection_established_ms());
    if connection_age_ms < 30_000 {
        // During startup grace period, only apply light penalties to prevent permanent
        // degradation
        return if conn.total_nak_count() == 0 {
            1.1
        } else {
            0.98
        };
    }

    let quality_mult = if let Some(nak_age_ms) = conn.time_since_last_nak_ms() {
        // Exponential decay for smooth, gradual recovery from NAKs
        // This replaces the step function with a continuous curve
        const HALF_LIFE_MS: f64 = 2000.0; // 2 second half-life
        const MAX_PENALTY: f64 = 0.5; // Start at 50% score (0.5x multiplier)

        // Exponential decay formula: penalty = max_penalty * e^(-age/half_life)
        // This gives smooth recovery:
        //   0ms:    50% penalty (0.5x multiplier)
        //   2000ms: 25% penalty (0.75x multiplier) -- half-life
        //   4000ms: 12.5% penalty (0.875x multiplier)
        //   6000ms: 6.25% penalty (0.9375x multiplier)
        //   8000ms+: ~0% penalty (1.0x multiplier)
        let decay_factor = (-(nak_age_ms as f64) / HALF_LIFE_MS).exp();
        let penalty = MAX_PENALTY * decay_factor;
        let mut mult = 1.0 - penalty; // Smoothly recovers from 0.5 to 1.0

        // Extra penalty for burst NAKs (multiple NAKs in short time)
        // Only apply if burst was significant (5+ NAKs) and recent (last 3s)
        if conn.nak_burst_count() >= 5 && nak_age_ms < 3000 {
            mult *= 0.7; // Moderate additional penalty for severe bursts
        }
        mult
    } else if conn.total_nak_count() == 0 {
        // Small bonus for connections that have never had NAKs
        1.1
    } else {
        // Had NAKs before but none recently tracked
        1.0
    };

    // RTT-Aware Scoring: Add small bonus for lower RTT connections
    // This helps differentiate connections when NAK patterns are similar
    let rtt_bonus = calculate_rtt_bonus(conn);
    quality_mult * rtt_bonus
}

/// Calculate RTT bonus for connection quality
/// Returns a multiplier between 1.0 (slow RTT) and 1.03 (fast RTT)
/// Bonus is very small to avoid causing instability
fn calculate_rtt_bonus(conn: &SrtlaConnection) -> f64 {
    let smooth_rtt = conn.get_smooth_rtt_ms();

    // Only apply RTT bonus if we have valid RTT measurements
    if smooth_rtt <= 0.0 {
        return 1.0; // No RTT data yet
    }

    // Prefer connections with RTT < 200ms
    // Formula: bonus = min(200ms / actual_rtt, 1.03)
    // Reduced from 1.05 to 1.03 to minimize switching impact
    // Examples:
    //   50ms RTT  -> 1.03x (capped at max bonus)
    //   100ms RTT -> 1.03x (capped)
    //   200ms RTT -> 1.00x (no bonus/penalty)
    //   400ms RTT -> 1.00x (no penalty, just no bonus)
    let rtt_factor = (200.0 / smooth_rtt.max(50.0)).min(1.03);

    // Only apply bonus, never penalty
    rtt_factor.max(1.0)
}

pub fn select_connection_idx(
    conns: &[SrtlaConnection],
    last_idx: Option<usize>,
    _last_switch_time_ms: u64,
    enable_quality: bool,
    enable_explore: bool,
    classic: bool,
) -> Option<usize> {
    // Classic mode: simple algorithm matching original implementation
    if classic {
        let mut best_idx: Option<usize> = None;
        let mut best_score: i32 = -1;

        for (i, c) in conns.iter().enumerate() {
            if c.is_timed_out() {
                continue;
            }
            let score = c.get_score();
            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
        return best_idx;
    }

    // Minimum switch interval DISABLED to allow natural load distribution
    // SRTLA works by selecting best connection per-packet, which naturally distributes load
    // Enforcing minimum interval prevents this natural distribution

    // Score connections by base score; apply quality multiplier unless classic
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
            let quality_mult = calculate_quality_multiplier(c);
            let final_score = (base * quality_mult).max(1.0);

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

    // Switching hysteresis: require new connection to be significantly better
    // REDUCED to 2% to allow better load distribution across multiple connections
    // Original 15% was preventing traffic from spreading across all uplinks
    const SWITCH_THRESHOLD: f64 = 1.02; // New connection must be 2% better

    if let (Some(last), Some(current)) = (last_idx, current_score) {
        // If current connection is still valid and new best isn't significantly better
        if best_idx != Some(last) && best_score < current * SWITCH_THRESHOLD {
            // Only log occasionally to reduce spam
            if now_ms() % 1000 < 10 {
                debug!(
                    "Hysteresis: staying with current connection (current: {:.1}, best: {:.1}, \
                     threshold: {:.1})",
                    current,
                    best_score,
                    current * SWITCH_THRESHOLD
                );
            }
            return Some(last);
        }
    }

    // Smarter Exploration: Context-aware exploration of alternative connections
    // Only explore when it makes sense based on connection quality patterns
    let explore_now = if enable_explore {
        should_explore_now(conns, best_idx, second_idx)
    } else {
        false
    };

    // Allow switching if better connection found (prevents getting stuck on
    // degraded connections)
    if explore_now {
        debug!("Exploration: trying second-best connection");
        second_idx.or(best_idx)
    } else {
        best_idx
    }
}

/// Determine if we should explore alternative connections
/// Returns true when exploration is likely to discover better options
fn should_explore_now(
    conns: &[SrtlaConnection],
    best_idx: Option<usize>,
    second_idx: Option<usize>,
) -> bool {
    // Need both best and second-best connections to explore
    let (best_idx, second_idx) = match (best_idx, second_idx) {
        (Some(b), Some(s)) => (b, s),
        _ => return false, // Not enough connections
    };

    if best_idx >= conns.len() || second_idx >= conns.len() {
        return false;
    }

    let best_conn = &conns[best_idx];
    let second_conn = &conns[second_idx];

    // Condition 1: Current best has recent NAKs (degrading)
    let best_degraded = best_conn
        .time_since_last_nak_ms()
        .map(|t| t < 3000)
        .unwrap_or(false);

    // Condition 2: Second-best has recovered from NAKs (potentially improved)
    let second_recovered = second_conn
        .time_since_last_nak_ms()
        .map(|t| t > 5000)
        .unwrap_or(true); // No NAKs = recovered

    // Condition 3: Periodic exploration as fallback (every 30s for 300ms)
    let periodic_exploration = (elapsed_ms() % 30000) < 300;

    // Explore if best is degraded AND second has recovered, OR periodic fallback
    (best_degraded && second_recovered) || periodic_exploration
}
