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
const ACK_INDEX_SIZE: usize = ACK_RING_SIZE * 2;

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
const NAK_INDEX_SIZE: usize = NAK_RING_SIZE * 2;

const EMPTY_BUCKET: u16 = u16::MAX;

/// Fast non-cryptographic 64-bit hash for packet deduplication.
/// Returns a non-zero hash so `0` can serve as the empty-slot sentinel.
#[inline]
fn hash_packet(packet: &[u8]) -> u64 {
    const K: u64 = 0x517cc1b727220a95;
    let mut h: u64 = (packet.len() as u64).wrapping_mul(K);
    let mut chunks = packet.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_ne_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        h = (h.rotate_left(5) ^ word).wrapping_mul(K);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut tail = 0u64;
        for (i, &b) in rem.iter().enumerate() {
            tail |= (b as u64) << (i * 8);
        }
        h = (h.rotate_left(5) ^ tail).wrapping_mul(K);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h | 1
}

#[derive(Clone, Copy, Default)]
struct Forwarded {
    /// Non-zero packet hash (`0` marks an empty slot).
    hash: u64,
    at_ms: u64,
}

/// Fixed-size FIFO ring with an open-addressing index table for O(1) lookup and
/// eviction. `N` and `M` must be powers of two with `M >= 2 * N` and
/// `N < u16::MAX as usize`.
struct Ring<const N: usize, const M: usize> {
    entries: [Forwarded; N],
    index: [u16; M],
    next: usize,
    window_ms: u64,
}

impl<const N: usize, const M: usize> Ring<N, M> {
    fn new(window_ms: u64) -> Self {
        Self {
            entries: [Forwarded::default(); N],
            index: [EMPTY_BUCKET; M],
            next: 0,
            window_ms,
        }
    }

    #[inline]
    fn remove_from_index(&mut self, hash: u64, target_slot: u16) {
        let mask = M - 1;
        let mut b = (hash as usize) & mask;
        while self.index[b] != EMPTY_BUCKET {
            if self.index[b] == target_slot {
                let mut empty = b;
                let mut cur = (b + 1) & mask;
                while self.index[cur] != EMPTY_BUCKET {
                    let cur_slot = self.index[cur] as usize;
                    let home = (self.entries[cur_slot].hash as usize) & mask;
                    let dist_cur = cur.wrapping_sub(home) & mask;
                    let dist_empty = empty.wrapping_sub(home) & mask;
                    if dist_empty < dist_cur {
                        self.index[empty] = self.index[cur];
                        empty = cur;
                    }
                    cur = (cur + 1) & mask;
                }
                self.index[empty] = EMPTY_BUCKET;
                return;
            }
            b = (b + 1) & mask;
        }
    }

    #[inline]
    fn insert_into_index(&mut self, hash: u64, slot: u16) {
        let mask = M - 1;
        let mut b = (hash as usize) & mask;
        while self.index[b] != EMPTY_BUCKET {
            b = (b + 1) & mask;
        }
        self.index[b] = slot;
    }

    #[inline]
    fn should_forward(&mut self, packet: &[u8], now_ms: u64) -> bool {
        let hash = hash_packet(packet);
        let mask = M - 1;

        let mut b = (hash as usize) & mask;
        while self.index[b] != EMPTY_BUCKET {
            let slot = self.index[b] as usize;
            let e = self.entries[slot];
            if e.hash == hash && now_ms.saturating_sub(e.at_ms) < self.window_ms {
                return false;
            }
            b = (b + 1) & mask;
        }

        let slot = self.next;
        let old = self.entries[slot];
        if old.hash != 0 {
            self.remove_from_index(old.hash, slot as u16);
        }

        self.entries[slot] = Forwarded {
            hash,
            at_ms: now_ms,
        };
        self.insert_into_index(hash, slot as u16);
        self.next = (slot + 1) & (N - 1);
        true
    }
}

/// Recently forwarded ACK and NAK packets, shared by every uplink of the bond.
pub struct ClientDedup {
    acks: Ring<ACK_RING_SIZE, ACK_INDEX_SIZE>,
    naks: Ring<NAK_RING_SIZE, NAK_INDEX_SIZE>,
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
