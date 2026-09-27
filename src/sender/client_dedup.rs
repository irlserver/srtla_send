//! One forward to the local SRT client per distinct SRT ACK or NAK.
//!
//! SRTLA receivers send SRT ACKs, and some also NAKs, down every uplink of the
//! bond, so one control packet can arrive once per uplink. libsrt answers every
//! full ACK with an ACKACK and retransmits for every NAK it reads, so relaying
//! each copy spends uplink capacity the bond needs for media: extra ACKACKs,
//! and repeat retransmits when a late NAK copy lands after the first one was
//! already served.
//!
//! Packets are keyed on their full bytes, SRT header timestamp included. Copies
//! of one packet are byte-identical, while a packet the SRT peer generates
//! afresh (a new ACK, or its own periodic NAK report) carries a new timestamp
//! and never matches an earlier one.
//!
//! Only the relay to the client is deduplicated. Link accounting (liveness,
//! window, in-flight, RTT) still runs on every copy, because each copy is also
//! proof that its own uplink is alive.

use std::hash::{DefaultHasher, Hash, Hasher};

/// How long a forwarded ACK suppresses byte-identical copies.
///
/// A byte-identical ACK is always a copy (full ACKs also carry a unique ACK
/// number), so this only has to outlast the downlink skew between the fastest
/// and slowest uplink, which bufferbloated cellular links can push to hundreds
/// of milliseconds.
pub const ACK_DEDUP_WINDOW_MS: u64 = 1000;

/// Distinct ACKs remembered. libsrt sends 100 full ACKs a second plus a light
/// ACK every 64 packets, so this covers the whole window up to about 26k
/// packets a second (over 250 Mbit/s). Overflow evicts the oldest entry, which
/// only lets a late copy through, as before deduplication.
const ACK_RING_SIZE: usize = 512;

/// How long a forwarded NAK suppresses byte-identical copies.
///
/// A byte-identical NAK is not always a copy. Moblin's SRTLA server re-sends
/// the latest NAK every 100 ms with the original timestamp, and srtla_rec's
/// optional periodic re-NAK does the same, so a repeat can match the previous
/// NAK byte for byte and still has to reach the client. The window stays well
/// under that period. The cost is that a NAK copy delayed past 50 ms on a slow
/// downlink is forwarded again; that can cause one redundant retransmit, the
/// safe failure next to swallowing a re-NAK.
pub const NAK_DEDUP_WINDOW_MS: u64 = 50;

/// Distinct NAKs remembered. Kept apart from the ACK ring so a burst of ACKs
/// cannot evict a NAK that is still inside its window.
const NAK_RING_SIZE: usize = 64;

#[derive(Clone, Copy)]
struct Forwarded {
    hash: u64,
    at_ms: u64,
}

struct Ring<const N: usize> {
    entries: [Option<Forwarded>; N],
    next: usize,
    window_ms: u64,
}

impl<const N: usize> Ring<N> {
    fn new(window_ms: u64) -> Self {
        Self {
            entries: [None; N],
            next: 0,
            window_ms,
        }
    }

    fn should_forward(&mut self, packet: &[u8], now_ms: u64) -> bool {
        let mut hasher = DefaultHasher::new();
        packet.hash(&mut hasher);
        let hash = hasher.finish();

        let seen_recently = self
            .entries
            .iter()
            .flatten()
            .any(|e| e.hash == hash && now_ms.saturating_sub(e.at_ms) < self.window_ms);
        if seen_recently {
            return false;
        }

        self.entries[self.next] = Some(Forwarded {
            hash,
            at_ms: now_ms,
        });
        self.next = (self.next + 1) % N;
        true
    }
}

/// Recently forwarded ACK and NAK packets, shared by every uplink of the bond.
pub struct ClientDedup {
    acks: Ring<ACK_RING_SIZE>,
    naks: Ring<NAK_RING_SIZE>,
}

impl Default for ClientDedup {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientDedup {
    pub fn new() -> Self {
        Self {
            acks: Ring::new(ACK_DEDUP_WINDOW_MS),
            naks: Ring::new(NAK_DEDUP_WINDOW_MS),
        }
    }

    /// Whether this SRT ACK should be forwarded to the client. Records it as
    /// forwarded when it is, so the caller must forward on `true`.
    pub fn should_forward_ack(&mut self, packet: &[u8], now_ms: u64) -> bool {
        self.acks.should_forward(packet, now_ms)
    }

    /// Whether this SRT NAK should be forwarded to the client. Records it as
    /// forwarded when it is, so the caller must forward on `true`.
    pub fn should_forward_nak(&mut self, packet: &[u8], now_ms: u64) -> bool {
        self.naks.should_forward(packet, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use srtla_protocol::{SRT_TYPE_ACK, SRT_TYPE_NAK};

    use super::*;

    fn control(srt_type: u16, timestamp: u32, body: u32) -> Vec<u8> {
        let mut pkt = srt_type.to_be_bytes().to_vec();
        pkt.resize(8, 0);
        pkt.extend_from_slice(&timestamp.to_be_bytes());
        pkt.resize(16, 0);
        pkt.extend_from_slice(&body.to_be_bytes());
        pkt
    }

    #[test]
    fn first_copy_is_forwarded() {
        let mut dedup = ClientDedup::new();
        assert!(dedup.should_forward_ack(&control(SRT_TYPE_ACK, 1, 7), 1_000));
        assert!(dedup.should_forward_nak(&control(SRT_TYPE_NAK, 1, 7), 1_000));
    }

    #[test]
    fn ack_copies_inside_the_window_are_dropped() {
        let mut dedup = ClientDedup::new();
        let ack = control(SRT_TYPE_ACK, 1, 7);
        assert!(dedup.should_forward_ack(&ack, 1_000));
        assert!(!dedup.should_forward_ack(&ack, 1_000));
        assert!(
            !dedup.should_forward_ack(&ack, 1_000 + ACK_DEDUP_WINDOW_MS - 1),
            "a copy on a slow downlink is still a copy"
        );
        assert!(dedup.should_forward_ack(&ack, 1_000 + ACK_DEDUP_WINDOW_MS));
    }

    #[test]
    fn a_different_packet_is_forwarded() {
        let mut dedup = ClientDedup::new();
        assert!(dedup.should_forward_ack(&control(SRT_TYPE_ACK, 1, 7), 1_000));
        assert!(dedup.should_forward_ack(&control(SRT_TYPE_ACK, 1, 8), 1_000));
        assert!(
            dedup.should_forward_ack(&control(SRT_TYPE_ACK, 2, 7), 1_000),
            "a fresh timestamp makes a new packet even with the same body"
        );
    }

    #[test]
    fn nak_repeat_inside_the_window_is_dropped_and_after_it_forwarded() {
        let mut dedup = ClientDedup::new();
        let nak = control(SRT_TYPE_NAK, 1, 500);
        assert!(dedup.should_forward_nak(&nak, 1_000));
        assert!(!dedup.should_forward_nak(&nak, 1_020));
        assert!(
            dedup.should_forward_nak(&nak, 1_000 + NAK_DEDUP_WINDOW_MS),
            "a byte-identical NAK after the window is a receiver's re-NAK"
        );
        assert!(
            !dedup.should_forward_nak(&nak, 1_000 + NAK_DEDUP_WINDOW_MS + 1),
            "the re-NAK's own copies are deduplicated"
        );
    }

    #[test]
    fn a_periodic_renak_at_100ms_reaches_the_client() {
        let mut dedup = ClientDedup::new();
        let nak = control(SRT_TYPE_NAK, 1, 500);
        assert!(dedup.should_forward_nak(&nak, 1_000));
        assert!(dedup.should_forward_nak(&nak, 1_100));
        assert!(dedup.should_forward_nak(&nak, 1_200));
    }

    #[test]
    fn acks_do_not_evict_naks() {
        let mut dedup = ClientDedup::new();
        let nak = control(SRT_TYPE_NAK, 1, 500);
        assert!(dedup.should_forward_nak(&nak, 1_000));
        for n in 0..=ACK_RING_SIZE as u32 {
            dedup.should_forward_ack(&control(SRT_TYPE_ACK, n, n), 1_000);
        }
        assert!(!dedup.should_forward_nak(&nak, 1_010));
    }

    #[test]
    fn ring_overflow_forgets_the_oldest_forward() {
        let mut dedup = ClientDedup::new();
        let first = control(SRT_TYPE_ACK, 0, 0);
        assert!(dedup.should_forward_ack(&first, 1_000));
        for n in 1..=ACK_RING_SIZE as u32 {
            assert!(dedup.should_forward_ack(&control(SRT_TYPE_ACK, n, n), 1_000));
        }
        assert!(
            dedup.should_forward_ack(&first, 1_000),
            "an evicted entry fails open: the copy is forwarded"
        );
    }
}
