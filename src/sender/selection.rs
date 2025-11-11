use tracing::debug;

use crate::connection::SrtlaConnection;
use crate::utils::{elapsed_ms, now_ms};

pub fn calculate_quality_multiplier(conn: &SrtlaConnection) -> f64 {
    // Startup grace period: first 10 seconds after connection establishment
    // During this time, use simple scoring like original C version to go live fast
    // This prevents early NAKs from permanently degrading connections
    let connection_age_ms = now_ms().saturating_sub(conn.connection_established_ms());
    if connection_age_ms < 10000 {
        // During startup grace period, only apply light penalties to prevent permanent
        // degradation
        return if conn.total_nak_count() == 0 {
            1.2
        } else {
            0.95
        };
    }

    if let Some(tsn) = conn.time_since_last_nak_ms() {
        let mut quality_mult = if tsn < 2000 {
            0.1
        } else if tsn < 5000 {
            0.5
        } else if tsn < 10_000 {
            0.8
        } else if conn.total_nak_count() == 0 {
            1.2
        } else {
            1.0
        };

        // Extra penalty for burst NAKs (multiple NAKs in short time)
        if conn.nak_burst_count() > 1 && tsn < 5000 {
            quality_mult *= 0.5; // Halve score for connections with NAK bursts
        }
        quality_mult
    } else if conn.total_nak_count() == 0 {
        // Bonus for connections that have never had NAKs
        1.2
    } else {
        1.0
    }
}

pub fn select_connection_idx(
    conns: &[SrtlaConnection],
    _last_idx: Option<usize>,
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

    // Exploration window: simple periodic exploration of second-best
    // Use elapsed time since program start for consistent periodic behavior
    // Every 5 seconds, explore for 300ms window
    let explore_now = enable_explore && (elapsed_ms() % 5000) < 300;
    // Score connections by base score; apply quality multiplier unless classic
    let mut best_idx: Option<usize> = None;
    let mut second_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;
    let mut second_score: f64 = -1.0;
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

    // Allow switching if better connection found (prevents getting stuck on
    // degraded connections)
    if explore_now {
        second_idx.or(best_idx)
    } else {
        best_idx
    }
}
