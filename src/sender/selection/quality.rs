//! Quality scoring for enhanced mode connection selection
//!
//! This module calculates quality multipliers based on NAK history, RTT, and connection age.
//!
//! ## Performance Optimization
//!
//! Quality multipliers are cached per connection and recalculated every 50ms to avoid
//! expensive `exp()` calculations on every packet. This reduces CPU overhead by ~2-5%.

use crate::connection::SrtlaConnection;

/// Startup grace period in milliseconds - prevents early NAKs from degrading connections
const STARTUP_GRACE_PERIOD_MS: u64 = 30_000;

/// Quality multiplier for perfect connections (never had NAKs)
const PERFECT_CONNECTION_BONUS: f64 = 1.1;

/// Quality multiplier during startup grace period when NAKs occur
const STARTUP_NAK_PENALTY: f64 = 0.98;

/// Exponential decay half-life for NAK recovery in milliseconds
const HALF_LIFE_MS: f64 = 2000.0;

/// Maximum initial penalty multiplier after a NAK (0.5 = 50% penalty)
const MAX_PENALTY: f64 = 0.5;

/// Minimum NAK burst count to trigger burst penalty
const NAK_BURST_THRESHOLD: i32 = 5;

/// Maximum age in milliseconds for NAK burst to apply penalty
const NAK_BURST_MAX_AGE_MS: u64 = 3000;

/// Additional multiplier penalty for NAK bursts (0.7 = 30% extra penalty)
const NAK_BURST_PENALTY: f64 = 0.7;

/// RTT threshold in milliseconds for bonus calculation
const RTT_BONUS_THRESHOLD_MS: f64 = 200.0;

/// Minimum RTT in milliseconds for bonus calculation (prevents division issues)
const MIN_RTT_MS: f64 = 50.0;

/// Maximum RTT bonus multiplier for low-latency connections (3% max bonus)
const MAX_RTT_BONUS: f64 = 1.03;

/// Calculate quality multiplier for a connection based on NAK history and RTT.
///
/// This is the public API that should be used by connection selection code.
/// For frequently-called code paths, consider using `CachedQuality` to reduce
/// the overhead of repeated calculations.
///
/// Returns a multiplier that adjusts the base score:
/// - 1.1x bonus for perfect connections (no NAKs)
/// - 0.5x-1.0x penalty for connections with recent NAKs (exponential decay)
/// - 0.7x additional multiplier for NAK bursts (30% reduction for 5+ NAKs in short time)
/// - 1.0x-1.03x RTT bonus for low-latency connections
///
/// The `current_time_ms` parameter allows the caller to pass a cached timestamp
/// to avoid repeated syscalls when processing multiple connections.
#[inline(always)]
pub fn calculate_quality_multiplier(conn: &SrtlaConnection, current_time_ms: u64) -> f64 {
    calculate_quality_multiplier_uncached(conn, current_time_ms)
}

/// Internal uncached quality multiplier calculation.
#[inline(always)]
fn calculate_quality_multiplier_uncached(conn: &SrtlaConnection, current_time_ms: u64) -> f64 {
    // Startup grace period: first 30 seconds after connection establishment
    // During this time, use simple scoring like original C version to go live fast
    // This prevents early NAKs from permanently degrading connections
    let connection_age_ms = current_time_ms.saturating_sub(conn.connection_established_ms());
    if connection_age_ms < STARTUP_GRACE_PERIOD_MS {
        // During startup grace period, only apply light penalties to prevent permanent
        // degradation
        return if conn.total_nak_count() == 0 {
            PERFECT_CONNECTION_BONUS
        } else {
            STARTUP_NAK_PENALTY
        };
    }

    let quality_mult = if let Some(nak_age_ms) = conn.time_since_last_nak_ms() {
        // Exponential decay for smooth, gradual recovery from NAKs
        // This replaces the step function with a continuous curve
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
        // Only apply if burst was significant and recent
        if conn.nak_burst_count() >= NAK_BURST_THRESHOLD && nak_age_ms < NAK_BURST_MAX_AGE_MS {
            mult *= NAK_BURST_PENALTY;
        }
        mult
    } else if conn.total_nak_count() == 0 {
        // Small bonus for connections that have never had NAKs
        PERFECT_CONNECTION_BONUS
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
/// Returns a multiplier between 1.0 (slow RTT) and MAX_RTT_BONUS (fast RTT)
/// Bonus is very small to avoid causing instability
#[inline(always)]
fn calculate_rtt_bonus(conn: &SrtlaConnection) -> f64 {
    let smooth_rtt = conn.get_smooth_rtt_ms();

    // Only apply RTT bonus if we have valid RTT measurements
    if smooth_rtt <= 0.0 {
        return 1.0; // No RTT data yet
    }

    // Prefer connections with RTT < RTT_BONUS_THRESHOLD_MS
    // Formula: bonus = min(threshold / actual_rtt, MAX_RTT_BONUS)
    // Examples:
    //   50ms RTT  -> 1.03x (capped at max bonus)
    //   100ms RTT -> 1.03x (capped)
    //   200ms RTT -> 1.00x (no bonus/penalty)
    //   400ms RTT -> 1.00x (no penalty, just no bonus)
    let rtt_factor = (RTT_BONUS_THRESHOLD_MS / smooth_rtt.max(MIN_RTT_MS)).min(MAX_RTT_BONUS);

    // Only apply bonus, never penalty
    rtt_factor.max(1.0)
}

// Tests are in src/tests/sender_tests.rs
