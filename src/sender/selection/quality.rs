//! Quality scoring for enhanced mode connection selection
//!
//! This module calculates quality multipliers based on NAK history, RTT, and connection age.

use crate::connection::SrtlaConnection;
use crate::utils::now_ms;

/// Calculate quality multiplier for a connection based on NAK history and RTT
///
/// Returns a multiplier that adjusts the base score:
/// - 1.1x bonus for perfect connections (no NAKs)
/// - 0.5x-1.0x penalty for connections with recent NAKs (exponential decay)
/// - 0.7x additional penalty for NAK bursts (5+ NAKs in short time)
/// - 1.0x-1.03x RTT bonus for low-latency connections
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
        //   0ms:     50% penalty (0.5x multiplier)
        //   2000ms:  18.4% penalty (0.816x multiplier)
        //   4000ms:   6.8% penalty (0.932x multiplier)
        //   6000ms:   2.5% penalty (0.975x multiplier)
        //   8000ms+: ~0.9% penalty (0.991x multiplier)
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
///
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

// Tests are in src/tests/sender_tests.rs
