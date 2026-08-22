use std::cmp::min;

use srtla_protocol::*;

use super::SrtlaConnection;
use crate::seq::{NO_ACK_YET, seq_after, seq_diff, seq_next, seq_normalize};

/// Widest newly-ACKed range that is cleared by walking sequence numbers instead
/// of sweeping the packet log.
///
/// The walk is O(range); the sweep is O(log entries), i.e. bounded by what is
/// actually in flight. Capping the walk here is what keeps a bogus far-future
/// ACK cheap: it can never make this loop run for a meaningful fraction of the
/// sequence space, it just falls into the sweep. (An ACK further than half the
/// space ahead does not even get that far — serial ordering reads it as stale
/// and `handle_srt_ack` returns early.)
const TARGETED_REMOVAL_LIMIT: i32 = 64;

impl SrtlaConnection {
    /// Register a packet as in-flight. O(1) insert.
    #[inline]
    pub fn register_packet(&mut self, seq: i32, send_time_ms: u64) {
        self.packet_log.insert(seq, send_time_ms);
        self.in_flight_packets = self.packet_log.len() as i32;
    }

    /// Handle SRT cumulative ACK - clears all packets at or before `ack`.
    ///
    /// "At or before" is meant in *serial* order (mod 2^31, see [`crate::seq`]):
    /// SRT sequence numbers are 31 bits and wrap from a randomized start, so
    /// numeric comparison would break for one full window every time a stream
    /// crosses `0x7fff_ffff`.
    ///
    /// Optimized to avoid redundant work:
    /// - Tracks highest_acked_seq to skip already-processed ACKs
    /// - Only removes packets in the range (highest_acked_seq, ack]
    /// - O(k) where k is packets in range, not O(n) for entire log
    ///
    /// `owns_acked_seq` says whether *this* link carried the unique copy of
    /// `ack`, and gates the RTT sample. An SRT cumulative ACK is a flow-level
    /// signal: the shell hands it to every link so they all prune, but it only
    /// proves delivery by whichever link actually carried the sequence. A link
    /// that holds `ack` merely as a duplicate probe would otherwise measure the
    /// round trip of the *healthy* link that delivered the real copy and record
    /// it as its own — a fast sample invented for a path that never delivered
    /// anything, exactly on the links whose lateness is the thing being
    /// measured. The shell resolves ownership through the sequence tracker,
    /// which deliberately never records probe copies.
    pub fn handle_srt_ack(&mut self, ack: i32, now_ms: u64, owns_acked_seq: bool) {
        // Reduce the wire word to the 31-bit space the packet log is keyed in.
        // A valid SRT ACK already has its MSB clear; masking a corrupt one keeps
        // it from posing as a sequence from the far half of the space.
        let ack = seq_normalize(ack);

        let old_highest = self.highest_acked_seq;
        let first_ack = old_highest == NO_ACK_YET;

        // Skip ACKs that don't advance our highest acked sequence — duplicates
        // and reordering. The comparison is *serial* (mod 2^31), not numeric:
        // SRT sequences wrap 0x7fff_ffff -> 0 from a randomized start, so a raw
        // `ack <= highest_acked_seq` declares every ACK after the wrap a
        // duplicate. That froze both the RTT samples and the in-flight
        // accounting below until some recovery reset the sentinel.
        if !first_ack && !seq_after(ack, old_highest) {
            return;
        }

        // Get send time for RTT calculation before removing
        let ack_send_time_ms = self.packet_log.get(&ack).copied();

        self.highest_acked_seq = ack;

        // Clear the newly-ACKed range (old_highest, ack]. Walking the range is
        // cheaper than sweeping the log while ACKs arrive in order; a large gap
        // (first ACK, or a jump after a reconnect) sweeps instead.
        let advance = if first_ack {
            // No lower bound to walk from: everything at or before `ack` goes.
            i32::MAX
        } else {
            // Positive: the early return above rejected everything else.
            seq_diff(ack, old_highest)
        };
        if advance <= TARGETED_REMOVAL_LIMIT {
            // Step the sequences rather than the map, wrapping at the top of
            // the space so a range straddling 0x7fff_ffff -> 0 removes the keys
            // the receiver actually acknowledged.
            let mut seq = old_highest;
            for _ in 0..advance {
                seq = seq_next(seq);
                self.packet_log.remove(&seq);
            }
        } else {
            // Keep only what is still ahead of `ack` in serial order.
            self.packet_log.retain(|&seq, _| seq_after(seq, ack));
        }
        self.in_flight_packets = self.packet_log.len() as i32;

        // Update RTT estimate if we found the acked packet *and* it was ours.
        if owns_acked_seq && let Some(sent_ms) = ack_send_time_ms {
            self.rtt.record_round_trip(sent_ms, now_ms);
        }
    }

    /// Handle NAK for a specific sequence. O(1) remove.
    #[inline]
    pub fn handle_nak(&mut self, seq: i32, now_ms: u64) -> bool {
        let found = self.packet_log.remove(&seq).is_some();
        if found {
            self.in_flight_packets = self.packet_log.len() as i32;
            self.congestion
                .handle_nak(&mut self.window, seq, &self.label, now_ms);
        }
        found
    }

    /// Handle SRTLA ACK for a specific sequence. O(1) remove.
    #[inline]
    pub fn handle_srtla_ack_specific(&mut self, seq: i32, classic_mode: bool, now_ms: u64) -> bool {
        // A probe this link sent, answered on this link. It proves the path
        // delivered a data-sized packet and yields a real round trip, but it is
        // not payload: it must not move the congestion window, or a held-out
        // link would inflate the very window that makes it seize the stream on
        // release. Checked first — the sweep-proof log is where a slow link's
        // probes actually survive to be answered.
        if let Some(sent_ms) = self.probe_log.remove(&seq) {
            self.last_ack_or_rtt_sample_ms = now_ms;
            self.rtt.record_round_trip(sent_ms, now_ms);
            return true;
        }

        let sent_ms = self.packet_log.remove(&seq);
        if let Some(sent_ms) = sent_ms {
            self.in_flight_packets = self.packet_log.len() as i32;

            // Delivery proof for `stall_deselect`: this link OWNED the acked seq,
            // the strongest per-link proof it is still moving data. Stamped here
            // and at the keepalive-RTT site only (see `packet_io.rs`), never on
            // generic inbound bytes, so a stalled-but-echoing link stays stale.
            self.last_ack_or_rtt_sample_ms = now_ms;

            // ...and the same ACK is a per-link round trip: we sent this exact
            // sequence on this exact socket and the receiver acknowledged it
            // back on it. Dropping the send timestamp here used to throw that
            // sample away.
            self.rtt.record_round_trip(sent_ms, now_ms);

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
                    now_ms,
                );
            }
        }
        sent_ms.is_some()
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

#[cfg(test)]
mod tests {
    use crate::seq::{NO_ACK_YET, SEQ_MASK};
    use crate::test_helpers::create_test_connection;

    /// Sequences around a wrap: the last two before `0x7fff_ffff` rolls over,
    /// then the first three of the new cycle.
    const ACROSS_WRAP: [i32; 5] = [SEQ_MASK - 1, SEQ_MASK, 0, 1, 2];

    #[tokio::test]
    async fn first_ack_after_sentinel_is_accepted() {
        let mut conn = create_test_connection().await;
        assert_eq!(conn.highest_acked_seq, NO_ACK_YET);

        conn.register_packet(10, 1_000);
        conn.register_packet(20, 1_000);
        conn.handle_srt_ack(10, 1_040, true);

        assert_eq!(conn.highest_acked_seq, 10);
        assert!(!conn.packet_log.contains_key(&10));
        assert!(conn.packet_log.contains_key(&20));
        assert_eq!(conn.in_flight_packets, 1);
        // The first ACK is also a valid round trip when this link owned it.
        assert_eq!(conn.rtt.estimated_rtt_ms, 40.0);
    }

    #[tokio::test]
    async fn duplicate_and_reordered_acks_are_ignored() {
        let mut conn = create_test_connection().await;
        for seq in [10, 20, 30] {
            conn.register_packet(seq, 1_000);
        }
        conn.handle_srt_ack(20, 1_040, true);
        assert_eq!(conn.in_flight_packets, 1);

        // Same ACK again, and an older one: neither may prune or re-sample.
        conn.handle_srt_ack(20, 5_000, true);
        conn.handle_srt_ack(10, 5_000, true);
        assert_eq!(conn.highest_acked_seq, 20);
        assert_eq!(conn.in_flight_packets, 1);
        assert!(conn.packet_log.contains_key(&30));
        assert_eq!(conn.rtt.estimated_rtt_ms, 40.0);
    }

    /// The regression: a stream crossing `0x7fff_ffff` must keep pruning and
    /// keep sampling RTT. Numeric comparison called every post-wrap ACK a
    /// duplicate and stalled both until the link was reset.
    #[tokio::test]
    async fn cumulative_ack_across_the_wrap_keeps_pruning_and_sampling() {
        let mut conn = create_test_connection().await;
        for seq in ACROSS_WRAP {
            conn.register_packet(seq, 1_000);
        }
        assert_eq!(conn.in_flight_packets, 5);

        // Last ACK before the wrap.
        conn.handle_srt_ack(SEQ_MASK - 1, 1_040, true);
        assert_eq!(conn.in_flight_packets, 4);
        assert_eq!(conn.rtt.estimated_rtt_ms, 40.0);

        // ...and the first one after it. One step forward, not a rollback.
        conn.handle_srt_ack(0, 1_060, true);
        assert_eq!(conn.highest_acked_seq, 0);
        assert!(!conn.packet_log.contains_key(&SEQ_MASK));
        assert!(!conn.packet_log.contains_key(&0));
        // (SEQ_MASK - 1, 0] covers both SEQ_MASK and 0, leaving 1 and 2.
        assert_eq!(conn.in_flight_packets, 2);
        // A fresh sample landed: the estimator kept moving across the wrap.
        assert!(conn.rtt.estimated_rtt_ms > 40.0);

        // A range straddling the wrap clears exactly (SEQ_MASK - 1, 2].
        conn.handle_srt_ack(2, 1_080, true);
        assert_eq!(conn.in_flight_packets, 0);
        assert!(conn.packet_log.is_empty());
    }

    #[tokio::test]
    async fn wrap_spanning_range_removes_only_the_acked_keys() {
        let mut conn = create_test_connection().await;
        for seq in ACROSS_WRAP {
            conn.register_packet(seq, 1_000);
        }
        // Untouched by (SEQ_MASK - 1, 2]: still ahead of the ACK.
        conn.register_packet(3, 1_000);
        conn.register_packet(50, 1_000);

        conn.handle_srt_ack(SEQ_MASK - 1, 1_010, false);
        conn.handle_srt_ack(2, 1_020, false);

        assert_eq!(conn.highest_acked_seq, 2);
        let mut left: Vec<i32> = conn.packet_log.keys().copied().collect();
        left.sort_unstable();
        assert_eq!(left, vec![3, 50]);
        assert_eq!(conn.in_flight_packets, 2);
    }

    #[tokio::test]
    async fn post_wrap_ack_is_not_a_duplicate_even_after_a_long_pre_wrap_run() {
        let mut conn = create_test_connection().await;
        conn.register_packet(SEQ_MASK, 1_000);
        conn.handle_srt_ack(SEQ_MASK, 1_010, false);
        assert_eq!(conn.highest_acked_seq, SEQ_MASK);

        // Well past the 64-sequence walk limit, so this takes the sweep path.
        conn.register_packet(500, 2_000);
        conn.register_packet(1_000, 2_000);
        conn.handle_srt_ack(500, 2_050, true);

        assert_eq!(conn.highest_acked_seq, 500);
        assert!(!conn.packet_log.contains_key(&500));
        assert!(conn.packet_log.contains_key(&1_000));
        assert_eq!(conn.rtt.estimated_rtt_ms, 50.0);
    }

    #[tokio::test]
    async fn far_future_ack_is_stale_and_prunes_nothing() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        conn.handle_srt_ack(10, 1_040, false);

        // More than half the sequence space ahead: ambiguous by construction,
        // so it is read as stale rather than as a giant forward jump.
        conn.register_packet(20, 1_000);
        conn.handle_srt_ack(0x4000_0010, 1_050, false);

        assert_eq!(conn.highest_acked_seq, 10);
        assert!(conn.packet_log.contains_key(&20));
        assert_eq!(conn.in_flight_packets, 1);
    }

    #[tokio::test]
    async fn corrupt_msb_ack_is_normalized_not_negative() {
        let mut conn = create_test_connection().await;
        conn.register_packet(30, 1_000);
        // 0x8000_001e on the wire: the MSB is invalid on an ACK number.
        conn.handle_srt_ack(i32::MIN | 30, 1_040, true);

        assert_eq!(conn.highest_acked_seq, 30);
        assert!(conn.packet_log.is_empty());
        assert_eq!(conn.rtt.estimated_rtt_ms, 40.0);
    }
}
