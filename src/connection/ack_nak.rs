use std::cmp::min;

use super::SrtlaConnection;
use crate::protocol::*;
use crate::utils::now_ms;

impl SrtlaConnection {
    /// Register a packet as in-flight. O(1) insert.
    #[inline]
    pub fn register_packet(&mut self, seq: i32, send_time_ms: u64) {
        self.packet_log.insert(seq, send_time_ms);
        self.in_flight_packets = self.packet_log.len() as i32;
    }

    /// Handle SRT cumulative ACK - clears all packets with seq <= ack.
    ///
    /// Optimized to avoid redundant work:
    /// - Tracks highest_acked_seq to skip already-processed ACKs
    /// - Only removes packets in the range (highest_acked_seq, ack]
    /// - O(k) where k is packets in range, not O(n) for entire log
    pub fn handle_srt_ack(&mut self, ack: i32) {
        // Skip if this ACK doesn't advance our highest acked sequence
        // This handles duplicate ACKs and out-of-order ACKs efficiently
        if ack <= self.highest_acked_seq {
            return;
        }

        // Get send time for RTT calculation before removing
        let ack_send_time_ms = self.packet_log.get(&ack).copied();

        // Remove packets in the range (highest_acked_seq, ack]
        // This is more efficient than retain() when ACKs arrive in order
        // because we only iterate over the newly-ACKed range
        let old_highest = self.highest_acked_seq;
        self.highest_acked_seq = ack;

        // For small ranges, use targeted removal (O(k) where k = range size)
        // For large gaps (e.g., after reconnect), fall back to retain (O(n))
        let range_size = (ack as i64 - old_highest as i64).unsigned_abs();
        if range_size <= 64 && old_highest != i32::MIN {
            // Targeted removal for small ranges - iterate the range, not the map
            for seq in (old_highest + 1)..=ack {
                self.packet_log.remove(&seq);
            }
        } else {
            // Fall back to retain for large gaps or initial state
            self.packet_log.retain(|&seq, _| seq > ack);
        }
        self.in_flight_packets = self.packet_log.len() as i32;

        // Update RTT estimate if we found the acked packet
        if let Some(sent_ms) = ack_send_time_ms {
            let now = now_ms();
            let rtt = now.saturating_sub(sent_ms);
            if rtt > 0 && rtt <= 10_000 {
                self.rtt.update_estimate(rtt);
            }
        }
    }

    /// Handle NAK for a specific sequence. O(1) remove.
    #[inline]
    pub fn handle_nak(&mut self, seq: i32) -> bool {
        let found = self.packet_log.remove(&seq).is_some();
        if found {
            self.in_flight_packets = self.packet_log.len() as i32;
            self.congestion
                .handle_nak(&mut self.window, seq, &self.label);
        }
        found
    }

    /// Handle SRTLA ACK for a specific sequence. O(1) remove.
    #[inline]
    pub fn handle_srtla_ack_specific(&mut self, seq: i32, classic_mode: bool) -> bool {
        let found = self.packet_log.remove(&seq).is_some();
        if found {
            self.in_flight_packets = self.packet_log.len() as i32;

            if classic_mode {
                self.congestion.handle_srtla_ack_specific_classic(
                    &mut self.window,
                    self.in_flight_packets,
                    seq,
                    &self.label,
                );
            } else {
                self.congestion.handle_srtla_ack_enhanced(
                    &mut self.window,
                    self.in_flight_packets,
                    &self.label,
                );
            }
        }
        found
    }

    pub fn handle_srtla_ack_global(&mut self) {
        // Global +1 window increase for connections that have received data (from
        // original implementation)
        // This matches C version: if (c->last_rcvd != 0)
        // In Rust, we check if last_received is Some (i.e., has been set when data was
        // received)
        if self.connected && self.last_received.is_some() {
            self.window = min(self.window + 1, WINDOW_MAX * WINDOW_MULT);
        }
    }
}
