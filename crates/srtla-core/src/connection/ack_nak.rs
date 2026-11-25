use std::cmp::min;

use smallvec::SmallVec;
use srtla_protocol::*;

use super::{PENDING_NAK_TIMEOUT_MS, PendingNak, SrtlaConnection};
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
        // A sequence this link already carries is an SRT retransmit on the same
        // path. Its ACK cannot say which copy arrived (Karn's ambiguity), and
        // timing an early copy's ACK against the resend's timestamp reads as a
        // near-zero RTT that pins the windowed floor. The entry stays for
        // accounting but stops supplying an RTT sample.
        self.packet_log
            .entry(seq)
            .and_modify(|sent| *sent = None)
            .or_insert(Some(send_time_ms));
        self.in_flight_packets = self.packet_log.len() as i32;
    }

    /// Handle SRT cumulative ACK - clears all packets at or before `ack`.
    ///
    /// `ack` is the last sequence the receiver acknowledged, not the ACK's
    /// wire field, which names the first sequence still owed; convert with
    /// [`crate::seq::srt_ack_last_acked`].
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
    /// which deliberately never records probe copies. The caller must also
    /// require that this copy of the ACK arrived on this link, because a copy
    /// on another uplink times that link's downlink instead.
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
        let ack_send_time_ms = self.packet_log.get(&ack).copied().flatten();

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
    ///
    /// Returns whether this link held `seq`. Classic mode charges every NAK it
    /// finds at once. In enhanced mode a NAK for a resend entry only clears it,
    /// and a NAK for a first send is parked in [`Self::pending_naks`] until
    /// this link's SRTLA ACKs show whether the packet was lost (see
    /// [`Self::handle_srtla_ack_specific`]) or [`Self::expire_pending_naks`]
    /// gives up waiting. Either way the entry leaves in-flight accounting now.
    #[inline]
    pub fn handle_nak(&mut self, seq: i32, classic_mode: bool, now_ms: u64) -> bool {
        let Some(entry) = self.packet_log.remove(&seq) else {
            return false;
        };
        self.in_flight_packets = self.packet_log.len() as i32;
        if classic_mode {
            // Parity with the reference srtla_send.c, resends included.
            self.congestion
                .handle_nak(&mut self.window, seq, &self.label, now_ms);
        } else if let Some(sent_ms) = entry {
            // The receiver reports a gap as soon as a later sequence arrives on
            // any link, so a sub-millisecond inversion between two links reads
            // as a loss here. The charge waits for evidence from this link.
            self.pending_naks.entry(seq).or_insert(PendingNak {
                sent_ms,
                nak_ms: now_ms,
            });
        }
        // A `None` entry in enhanced mode is a resend on this link. The
        // receiver repeats its loss report until the retransmit arrives, so a
        // NAK here usually restates a loss that was already charged.
        true
    }

    /// Charge a pending NAK as a loss at the time the NAK arrived, so burst
    /// detection sees the real spacing between losses.
    fn charge_pending_nak(&mut self, seq: i32, pending: PendingNak) {
        self.congestion
            .handle_nak(&mut self.window, seq, &self.label, pending.nak_ms);
    }

    /// Remove the pending NAKs `is_lost` selects and charge them in serial
    /// sequence order.
    fn charge_pending_naks_where(&mut self, is_lost: impl Fn(i32, &PendingNak) -> bool) {
        let mut lost: SmallVec<(i32, PendingNak), 8> = self
            .pending_naks
            .iter()
            .filter(|&(&seq, pending)| is_lost(seq, pending))
            .map(|(&seq, &pending)| (seq, pending))
            .collect();
        let Some(&(anchor, _)) = lost.first() else {
            return;
        };
        lost.sort_unstable_by_key(|&(seq, _)| seq_diff(seq, anchor));
        for (seq, pending) in lost {
            self.pending_naks.remove(&seq);
            self.charge_pending_nak(seq, pending);
        }
    }

    /// Charge every pending NAK that has waited [`PENDING_NAK_TIMEOUT_MS`]
    /// without this link's SRTLA ACKs settling it.
    pub fn expire_pending_naks(&mut self, now_ms: u64) {
        if self.pending_naks.is_empty() {
            return;
        }
        self.charge_pending_naks_where(|_, pending| {
            now_ms.saturating_sub(pending.nak_ms) >= PENDING_NAK_TIMEOUT_MS
        });
    }

    /// What this link logged for `seq`: `Some(Some(send_ms))` for a first send,
    /// `Some(None)` for a resend, `None` when the link does not hold it.
    pub fn logged_send(&self, seq: i32) -> Option<Option<u64>> {
        self.packet_log.get(&seq).copied()
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
        if let Some(probe) = self.probe_log.remove(&seq) {
            self.note_delivery_proof(now_ms);
            // A sequence probed twice on this link has no unambiguous send time.
            if !probe.ambiguous {
                self.rtt.record_round_trip(probe.sent_ms, now_ms);
            }
            return true;
        }

        // Every SRTLA receiver acknowledges each packet on the link it arrived
        // on, listing that link's sequences in arrival order (BELABOX
        // srtla_rec and irlserver srtla_rec both send one ACK per batch of
        // received packets). So an ACK on this link for a pending NAK proves
        // the packet arrived and the NAK was a cross-link reorder. An ACK for a
        // later sequence on this link, while the pending one never shows up,
        // proves it was lost here, because one link delivers in order.
        if let Some(pending) = self.pending_naks.remove(&seq) {
            self.note_delivery_proof(now_ms);
            // A resend logged after the NAK makes the ACK ambiguous between
            // the two copies, so it yields no round trip.
            if self.packet_log.remove(&seq).is_some() {
                self.in_flight_packets = self.packet_log.len() as i32;
            } else {
                self.rtt.record_round_trip(pending.sent_ms, now_ms);
            }
            // The packet left in-flight accounting at the NAK, so the window
            // does not grow for it.
            return true;
        }

        let logged = self.packet_log.remove(&seq);
        if let Some(sent_ms) = logged {
            self.in_flight_packets = self.packet_log.len() as i32;

            // Delivery proof for `stall_deselect`: this link OWNED the acked seq,
            // the strongest per-link proof it is still moving data. The other
            // proof sites are an SRTLA ACK arriving on a registered link
            // (`note_srtla_ack_proof`) and a keepalive echo
            // (`handle_keepalive_echo`), never generic inbound bytes.
            self.note_delivery_proof(now_ms);

            // ...and the same ACK is a per-link round trip: we sent this exact
            // sequence on this exact socket and the receiver acknowledged it
            // back on it. Dropping the send timestamp here used to throw that
            // sample away. A same-link resend has no unambiguous send time.
            if let Some(sent_ms) = sent_ms {
                self.rtt.record_round_trip(sent_ms, now_ms);
            }

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

            if !self.pending_naks.is_empty() {
                self.charge_pending_naks_where(|pending_seq, _| seq_after(seq, pending_seq));
            }
        }
        logged.is_some()
    }

    /// Bookkeeping for an SRTLA ACK that arrived on a *different* link but
    /// whose sequence this link still logs (the fallback scan in the packet
    /// handler). O(1) remove.
    ///
    /// The receiver answers on the link that delivered the packet, so a
    /// foreign-arrival ACK proves the *arrival* link, not this one: it must not
    /// stamp delivery proof, record a round trip, or grow the window here. Only
    /// the in-flight accounting moves.
    ///
    /// `probe_log` is deliberately left alone. A silence-pulled or stall-gated
    /// link's duplicate probe shares its sequence with the unique copy on the
    /// healthy link; once a cumulative SRT ACK clears that copy, the healthy
    /// link's SRTLA ACK lands here. Crediting the probe would keep a
    /// black-holed link's proof fresh forever, and consuming it would leave
    /// the probe's own ACK nothing to match, starving a live link of the proof
    /// and RTT it needs to finish its rejoin dwell. The probe waits for its
    /// own ACK, or ages out in `record_probe`.
    #[inline]
    pub fn handle_srtla_ack_foreign(&mut self, seq: i32) -> bool {
        if self.packet_log.remove(&seq).is_some() {
            self.in_flight_packets = self.packet_log.len() as i32;
            return true;
        }
        false
    }

    /// Forget the cumulative ACK state of the previous SRT session.
    ///
    /// The shell calls this when the encoder opens a new SRT session while the
    /// bond stays up. `highest_acked_seq` goes because the new session's random
    /// ISN can sit behind the old value in serial order, which makes
    /// `handle_srt_ack` ignore every ACK. `packet_log` goes because its entries
    /// belong to a session the encoder ended. The new session's ACKs can never
    /// prune them, so they would count as in flight until an SRTLA ACK happened
    /// to arrive for each. `probe_log` and `pending_naks` go because their keys
    /// are sequences of the ended session, so a new-session ACK could match
    /// one by accident and record a false RTT sample or a false loss. The
    /// congestion window, RTT and phase stay, because they describe the link,
    /// not the session.
    pub fn reset_srt_session(&mut self) {
        self.highest_acked_seq = NO_ACK_YET;
        self.packet_log.clear();
        self.probe_log.clear();
        self.pending_naks.clear();
        self.in_flight_packets = 0;
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
    use crate::connection::PENDING_NAK_TIMEOUT_MS;
    use crate::seq::{NO_ACK_YET, SEQ_MASK};
    use crate::test_helpers::create_test_connection;

    /// Sequences around a wrap: the last two before `0x7fff_ffff` rolls over,
    /// then the first three of the new cycle.
    const ACROSS_WRAP: [i32; 5] = [SEQ_MASK - 1, SEQ_MASK, 0, 1, 2];

    #[tokio::test]
    async fn same_link_resend_ack_keeps_rtt_and_still_accounts() {
        for srtla_ack in [false, true] {
            let mut conn = create_test_connection().await;
            conn.rtt.update_estimate(50, 950);
            conn.register_packet(30, 1_000);
            conn.register_packet(30, 1_029);
            conn.register_packet(31, 1_030);

            // The first copy's ACK lands 2 ms after the resend, faster than the
            // 50 ms path could carry the resend.
            if srtla_ack {
                assert!(conn.handle_srtla_ack_specific(30, false, 1_031));
                assert_eq!(conn.last_ack_or_rtt_sample_ms, 1_031);
            } else {
                conn.handle_srt_ack(30, 1_031, true);
            }

            assert!(!conn.packet_log.contains_key(&30));
            assert_eq!(conn.in_flight_packets, 1);
            assert_eq!(conn.get_rtt_min_ms(), 50.0);
            assert_eq!(conn.get_smooth_rtt_ms(), 50.0);
        }
    }

    #[tokio::test]
    async fn retransmit_after_nak_ack_keeps_rtt_and_still_accounts() {
        for srtla_ack in [false, true] {
            let mut conn = create_test_connection().await;
            conn.rtt.update_estimate(50, 950);
            conn.register_packet(30, 1_000);
            // A reorder NAK clears the original from the log while it is still
            // in flight; SRT resends it with the retransmit flag set.
            assert!(conn.handle_nak(30, false, 1_020));
            let mut packet = [0u8; 16];
            packet[..4].copy_from_slice(&30u32.to_be_bytes());
            srtla_protocol::SrtData::set_retransmit(&mut packet);
            conn.queue_data_packet(&packet, Some(30), 1_029);
            assert_eq!(conn.take_batch(1_029).len(), 1);
            conn.register_packet(31, 1_030);

            // The original's ACK lands 2 ms after the resend.
            if srtla_ack {
                assert!(conn.handle_srtla_ack_specific(30, false, 1_031));
            } else {
                conn.handle_srt_ack(30, 1_031, true);
            }

            assert!(!conn.packet_log.contains_key(&30));
            assert_eq!(conn.in_flight_packets, 1);
            assert_eq!(conn.get_rtt_min_ms(), 50.0);
            assert_eq!(conn.get_smooth_rtt_ms(), 50.0);
        }
    }

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
    async fn reset_srt_session_accepts_an_isn_behind_the_old_ack() {
        let mut conn = create_test_connection().await;
        let mut control = create_test_connection().await;
        for c in [&mut conn, &mut control] {
            c.register_packet(10, 1_000);
            c.register_packet(20, 1_000);
            c.handle_srt_ack(20, 1_040, false);
            assert_eq!(c.highest_acked_seq, 20);
        }

        conn.reset_srt_session();

        // More than 2^30 ahead of 20, so serial order reads it as behind the
        // old session's highest ACK.
        for c in [&mut conn, &mut control] {
            c.register_packet(0x4000_0100, 2_000);
            c.handle_srt_ack(0x4000_0100, 2_040, false);
        }

        assert_eq!(conn.highest_acked_seq, 0x4000_0100);
        assert!(conn.packet_log.is_empty());
        assert_eq!(conn.in_flight_packets, 0);

        assert_eq!(control.highest_acked_seq, 20);
        assert!(control.packet_log.contains_key(&0x4000_0100));
    }

    #[tokio::test]
    async fn reset_srt_session_clears_probe_log_and_keeps_window() {
        let mut conn = create_test_connection().await;
        conn.probe_log.insert(
            42,
            crate::connection::ProbeRecord {
                sent_ms: 1_000,
                ambiguous: false,
            },
        );
        conn.window = 12_345;

        conn.reset_srt_session();

        assert!(conn.probe_log.is_empty());
        assert_eq!(conn.window, 12_345);
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

    /// A foreign-arrival SRTLA ACK (fallback scan) clears the owner's log
    /// entry but must not refresh its delivery proof or take an RTT sample:
    /// the receiver answered on another link, so this link proved nothing.
    #[tokio::test]
    async fn foreign_ack_does_not_stamp_delivery_proof() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        conn.last_ack_or_rtt_sample_ms = 500;
        let rtt_before = conn.rtt.estimated_rtt_ms;

        assert!(conn.handle_srtla_ack_foreign(10));
        assert!(!conn.packet_log.contains_key(&10));
        assert_eq!(conn.in_flight_packets, 0);
        assert_eq!(conn.last_ack_or_rtt_sample_ms, 500, "proof must not move");
        assert_eq!(conn.rtt.estimated_rtt_ms, rtt_before, "no RTT sample");
    }

    /// A pulled link's duplicate probe sits in its probe log while the unique
    /// copy goes out on the healthy link. The healthy link's ACK falling back
    /// here must neither credit the probe nor consume it: the probe's own ACK,
    /// arriving later on this link, is still the proof.
    #[tokio::test]
    async fn foreign_ack_for_a_probe_leaves_it_for_its_own_ack() {
        let mut conn = create_test_connection().await;
        conn.probe_log.insert(
            42,
            crate::connection::ProbeRecord {
                sent_ms: 1_000,
                ambiguous: false,
            },
        );
        conn.last_ack_or_rtt_sample_ms = 500;

        assert!(!conn.handle_srtla_ack_foreign(42));
        assert!(conn.probe_log.contains_key(&42));
        assert_eq!(conn.last_ack_or_rtt_sample_ms, 500, "proof must not move");

        assert!(conn.handle_srtla_ack_specific(42, false, 1_040));
        assert_eq!(conn.last_ack_or_rtt_sample_ms, 1_040);
    }

    /// Unknown sequences are left for the next link in the fallback scan.
    #[tokio::test]
    async fn foreign_ack_for_an_unknown_seq_is_not_claimed() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        assert!(!conn.handle_srtla_ack_foreign(99));
        assert_eq!(conn.in_flight_packets, 1);
    }

    /// Arrival-link ACKs are unchanged: they still stamp proof.
    #[tokio::test]
    async fn arrival_ack_still_stamps_delivery_proof() {
        let mut conn = create_test_connection().await;
        conn.probe_log.insert(
            42,
            crate::connection::ProbeRecord {
                sent_ms: 1_000,
                ambiguous: false,
            },
        );
        assert!(conn.handle_srtla_ack_specific(42, false, 1_040));
        assert_eq!(conn.last_ack_or_rtt_sample_ms, 1_040);
    }

    /// A sequence probed twice on one link cannot time a round trip: its ACK
    /// may answer either copy. It still proves the link delivers.
    #[tokio::test]
    async fn a_repeated_probe_stamps_proof_but_takes_no_rtt_sample() {
        let mut conn = create_test_connection().await;
        conn.queue_probe_packet(&[0u8; 64], 42, 1_000);
        conn.queue_probe_packet(&[0u8; 64], 42, 1_500);
        assert!(conn.probe_log[&42].ambiguous);

        assert!(conn.handle_srtla_ack_specific(42, false, 1_510));
        assert_eq!(conn.last_ack_or_rtt_sample_ms, 1_510);
        assert_eq!(conn.rtt.estimated_rtt_ms, 0.0, "no RTT sample");
    }

    #[tokio::test]
    async fn a_single_probe_still_yields_an_rtt_sample() {
        let mut conn = create_test_connection().await;
        conn.queue_probe_packet(&[0u8; 64], 42, 1_000);

        assert!(conn.handle_srtla_ack_specific(42, false, 1_040));
        assert_eq!(conn.rtt.estimated_rtt_ms, 40.0);
    }

    /// A same-link repeat leaves a resend entry, the shape a retransmit-flagged
    /// copy also takes in the log.
    async fn connection_with_resend_entry() -> crate::connection::SrtlaConnection {
        let mut conn = create_test_connection().await;
        conn.register_packet(30, 1_000);
        conn.register_packet(30, 1_010);
        assert_eq!(conn.packet_log.get(&30), Some(&None));
        assert_eq!(conn.in_flight_packets, 1);
        conn
    }

    #[tokio::test]
    async fn logged_send_reports_first_sends_and_resends() {
        let mut conn = create_test_connection().await;
        conn.register_packet(5, 1_000);
        assert_eq!(conn.logged_send(5), Some(Some(1_000)));

        conn.register_packet(5, 1_010);
        assert_eq!(conn.logged_send(5), Some(None));

        assert_eq!(conn.logged_send(6), None);
    }

    #[tokio::test]
    async fn nak_for_a_resend_entry_is_not_charged_in_enhanced_mode() {
        let mut conn = connection_with_resend_entry().await;
        let window = conn.window;
        let naks = conn.congestion.nak_count;

        assert!(conn.handle_nak(30, false, 1_020));

        assert!(!conn.packet_log.contains_key(&30));
        assert_eq!(conn.in_flight_packets, 0);
        assert_eq!(conn.congestion.nak_count, naks);
        assert_eq!(conn.window, window);
    }

    #[tokio::test]
    async fn nak_for_a_resend_entry_is_still_charged_in_classic_mode() {
        let mut conn = connection_with_resend_entry().await;
        let window = conn.window;
        let naks = conn.congestion.nak_count;

        assert!(conn.handle_nak(30, true, 1_020));

        assert!(!conn.packet_log.contains_key(&30));
        assert_eq!(conn.in_flight_packets, 0);
        assert_eq!(conn.congestion.nak_count, naks + 1);
        assert!(conn.window < window);
    }

    #[tokio::test]
    async fn nak_for_a_first_send_is_charged_in_both_modes() {
        for classic_mode in [false, true] {
            let mut conn = create_test_connection().await;
            conn.register_packet(31, 1_000);
            let window = conn.window;
            let naks = conn.congestion.nak_count;

            assert!(conn.handle_nak(31, classic_mode, 1_020));
            // Enhanced mode charges a first send once the NAK times out.
            conn.expire_pending_naks(1_020 + PENDING_NAK_TIMEOUT_MS);

            assert!(!conn.packet_log.contains_key(&31));
            assert_eq!(conn.in_flight_packets, 0);
            assert_eq!(conn.congestion.nak_count, naks + 1);
            assert!(conn.window < window);
        }
    }

    #[tokio::test]
    async fn enhanced_nak_is_not_charged_before_evidence() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        let window = conn.window;
        let naks = conn.congestion.nak_count;

        assert!(conn.handle_nak(10, false, 1_040));

        assert_eq!(conn.congestion.nak_count, naks);
        assert_eq!(conn.window, window);
        assert_eq!(conn.in_flight_packets, 0);
        assert!(conn.pending_naks.contains_key(&10));
    }

    #[tokio::test]
    async fn late_srtla_ack_clears_a_pending_nak_as_reorder() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        let window = conn.window;
        let naks = conn.congestion.nak_count;
        assert!(conn.handle_nak(10, false, 1_040));

        assert!(conn.handle_srtla_ack_specific(10, false, 1_045));

        assert!(conn.pending_naks.is_empty());
        assert_eq!(conn.congestion.nak_count, naks);
        assert_eq!(conn.window, window);
        assert_eq!(conn.last_ack_or_rtt_sample_ms, 1_045);
        // The packet did arrive, so its ACK is a real round trip on this link.
        assert_eq!(conn.rtt.estimated_rtt_ms, 45.0);
    }

    #[tokio::test]
    async fn ack_for_a_later_packet_charges_the_pending_nak() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        conn.register_packet(11, 1_000);
        let naks = conn.congestion.nak_count;
        assert!(conn.handle_nak(10, false, 1_040));
        let window = conn.window;

        assert!(conn.handle_srtla_ack_specific(11, false, 1_050));

        assert_eq!(conn.congestion.nak_count, naks + 1);
        assert!(conn.window < window);
        assert!(conn.pending_naks.is_empty());
    }

    #[tokio::test]
    async fn ack_for_an_earlier_packet_leaves_the_pending_nak() {
        let mut conn = create_test_connection().await;
        conn.register_packet(9, 1_000);
        conn.register_packet(10, 1_000);
        let naks = conn.congestion.nak_count;
        assert!(conn.handle_nak(10, false, 1_040));

        assert!(conn.handle_srtla_ack_specific(9, false, 1_045));

        assert_eq!(conn.congestion.nak_count, naks);
        assert!(conn.pending_naks.contains_key(&10));
    }

    #[tokio::test]
    async fn pending_naks_across_the_wrap_are_charged_in_serial_order() {
        let mut conn = create_test_connection().await;
        for seq in ACROSS_WRAP {
            conn.register_packet(seq, 1_000);
        }
        // NAK times rise with serial order, so charging in that order keeps
        // the last charge at the latest NAK.
        for (seq, nak_ms) in [
            (SEQ_MASK - 1, 1_010),
            (SEQ_MASK, 1_020),
            (0, 1_030),
            (1, 1_040),
        ] {
            assert!(conn.handle_nak(seq, false, nak_ms));
        }

        assert!(conn.handle_srtla_ack_specific(2, false, 1_050));

        assert!(conn.pending_naks.is_empty());
        assert_eq!(conn.congestion.nak_count, 4);
        assert_eq!(conn.congestion.last_nak_time_ms, 1_040);
    }

    #[tokio::test]
    async fn pending_nak_expires_into_a_charge() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        let naks = conn.congestion.nak_count;
        assert!(conn.handle_nak(10, false, 1_040));

        conn.expire_pending_naks(1_040 + PENDING_NAK_TIMEOUT_MS - 1);
        assert_eq!(conn.congestion.nak_count, naks);
        assert!(conn.pending_naks.contains_key(&10));

        conn.expire_pending_naks(1_040 + PENDING_NAK_TIMEOUT_MS);
        assert_eq!(conn.congestion.nak_count, naks + 1);
        assert!(conn.pending_naks.is_empty());
    }

    #[tokio::test]
    async fn classic_nak_is_charged_at_once() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        let window = conn.window;
        let naks = conn.congestion.nak_count;

        assert!(conn.handle_nak(10, true, 1_040));

        assert_eq!(conn.congestion.nak_count, naks + 1);
        assert!(conn.window < window);
        assert!(conn.pending_naks.is_empty());
    }

    #[tokio::test]
    async fn reset_clears_pending_naks() {
        let mut conn = create_test_connection().await;
        conn.register_packet(10, 1_000);
        assert!(conn.handle_nak(10, false, 1_040));
        assert!(!conn.pending_naks.is_empty());

        conn.reset_srt_session();

        assert!(conn.pending_naks.is_empty());
    }
}
