use std::collections::{HashMap, VecDeque};

use crate::utils::now_ms;

pub const MAX_SEQUENCE_TRACKING: usize = 10_000;
pub const SEQUENCE_TRACKING_MAX_AGE_MS: u64 = 5000;
pub const SEQUENCE_MAP_CLEANUP_INTERVAL_MS: u64 = 5000;

pub struct SequenceTrackingEntry {
    pub conn_id: u64,
    pub timestamp_ms: u64,
}

impl SequenceTrackingEntry {
    pub fn is_expired(&self, current_time_ms: u64) -> bool {
        current_time_ms.saturating_sub(self.timestamp_ms) > SEQUENCE_TRACKING_MAX_AGE_MS
    }
}

pub fn cleanup_expired_sequence_tracking(
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_cleanup_ms: &mut u64,
) {
    use tracing::{debug, info, warn};

    let current_time = now_ms();
    if current_time.saturating_sub(*last_cleanup_ms) < SEQUENCE_MAP_CLEANUP_INTERVAL_MS {
        return;
    }
    *last_cleanup_ms = current_time;

    let before_size = seq_to_conn.len();
    let mut removed_count = 0;

    seq_to_conn.retain(|_seq, entry| {
        if entry.is_expired(current_time) {
            removed_count += 1;
            false
        } else {
            true
        }
    });

    seq_order.retain(|seq| seq_to_conn.contains_key(seq));

    if removed_count > 0 {
        let utilization = (seq_to_conn.len() as f64 / MAX_SEQUENCE_TRACKING as f64) * 100.0;
        if utilization >= 75.0 {
            info!(
                "Cleaned up {} stale sequence mappings ({} → {}, {:.1}% capacity)",
                removed_count,
                before_size,
                seq_to_conn.len(),
                utilization
            );
        } else {
            debug!(
                "Cleaned up {} stale sequence mappings ({} → {}, {:.1}% capacity)",
                removed_count,
                before_size,
                seq_to_conn.len(),
                utilization
            );
        }
    }

    if seq_to_conn.len() > (MAX_SEQUENCE_TRACKING as f64 * 0.8) as usize {
        warn!(
            "Sequence tracking at {:.1}% capacity ({}/{}) - consider review",
            (seq_to_conn.len() as f64 / MAX_SEQUENCE_TRACKING as f64) * 100.0,
            seq_to_conn.len(),
            MAX_SEQUENCE_TRACKING
        );
    }
}
