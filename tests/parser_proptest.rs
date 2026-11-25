//! Property-based fuzzing of the SRT/SRTLA packet parsers.
//!
//! Two contracts are exercised against generated inputs:
//!
//! 1. ROBUSTNESS — for ARBITRARY byte slices the parsers must never panic
//!    (no index-out-of-bounds, no slice-range panic, no unwrap), and the
//!    `SmallVec`-returning parsers must produce a provably bounded number of
//!    entries.
//! 2. ROUND-TRIP — for well-formed packets emitted by the builders,
//!    `parse(build(x)) == x`.
//!
//! These tests treat the parsers as a black box via the public `protocol`
//! re-exports; they add no test-only seams and assert nothing about parser
//! internals, so they cannot drift from production behavior. Input sizes are
//! capped (≤ 256 bytes, range/list lengths small) to keep every case cheap
//! while still covering all length/type branches.

use proptest::prelude::*;
use srtla_protocol::{
    ConnectionInfo, ControlPacket, Keepalive, RegPacket, SRT_TYPE_ACK, SRT_TYPE_NAK, SRTLA_ID_LEN,
    SrtData, SrtlaAck,
};

/// Cap arbitrary inputs at 256 bytes: enough to reach every length branch and
/// to let proptest synthesize NAK range words, while keeping each case fast.
const MAX_INPUT: usize = 256;

/// A NAK's loss list is the control packet's CIF: it starts after the full
/// 16-byte SRT control header, not after the 4-byte type word. Frames built for
/// these tests carry a header of the right size so the first loss entry lands
/// where the parser looks for it.
fn nak_header() -> Vec<u8> {
    let mut buf = vec![0u8; srtla_protocol::SRT_CONTROL_HEADER_LEN];
    buf[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());
    buf
}

prop_compose! {
    fn arb_conn_info()(
        conn_id in any::<u32>(),
        window in any::<i32>(),
        in_flight in any::<i32>(),
        rtt_ms in any::<u32>(),
        nak_count in any::<u32>(),
        bitrate_bytes_per_sec in any::<u32>(),
    ) -> ConnectionInfo {
        ConnectionInfo { conn_id, window, in_flight, rtt_ms, nak_count, bitrate_bytes_per_sec }
    }
}

proptest! {
    // ---- ROBUSTNESS: arbitrary bytes never panic, results are bounded ----

    /// `parse_srt_nak` on arbitrary bytes never panics and never indexes OOB.
    /// Upper bound: each 4-byte word yields at most one single ack, and range
    /// expansion is internally capped at 1000 total entries, so the result is
    /// at most `buf.len()/4 + 1000`.
    #[test]
    fn parse_srt_nak_never_panics_and_is_bounded(buf in prop::collection::vec(any::<u8>(), 0..MAX_INPUT)) {
        let out = ControlPacket::parse_srt_nak(&buf);
        prop_assert!(out.len() <= buf.len() / 4 + 1000);
    }

    /// Same, but biased toward real NAK frames (correct type byte) so the
    /// range/single decode branches are exercised far more often.
    #[test]
    fn parse_srt_nak_typed_never_panics_and_is_bounded(payload in prop::collection::vec(any::<u8>(), 0..MAX_INPUT)) {
        let mut buf = nak_header();
        buf.extend_from_slice(&payload);
        let out = ControlPacket::parse_srt_nak(&buf);
        prop_assert!(out.len() <= buf.len() / 4 + 1000);
    }

    /// `SrtlaAck::parse` on arbitrary bytes never panics / never indexes OOB,
    /// and emits at most one u32 per 4 bytes consumed.
    #[test]
    fn parse_srtla_ack_never_panics_and_is_bounded(buf in prop::collection::vec(any::<u8>(), 0..MAX_INPUT)) {
        let out = SrtlaAck::parse(&buf);
        prop_assert!(out.len() <= buf.len() / 4);
    }

    /// Packet-type detection and the scalar parsers never panic on arbitrary
    /// bytes regardless of length or content.
    #[test]
    fn type_detection_never_panics(buf in prop::collection::vec(any::<u8>(), 0..MAX_INPUT)) {
        let _ = ControlPacket::packet_type(&buf);
        let _ = SrtData::sequence_number(&buf);
        let _ = ControlPacket::parse_srt_ack(&buf);
        let _ = Keepalive::timestamp(&buf);
        let _ = Keepalive::conn_info(&buf);
        let _ = ControlPacket::is_srt_ack(&buf);
        let _ = Keepalive::is_keepalive(&buf);
        let _ = RegPacket::is_reg1(&buf);
        let _ = RegPacket::is_reg2(&buf);
        // packet_type agrees with the leading 2 bytes whenever present.
        if buf.len() >= 2 {
            prop_assert_eq!(ControlPacket::packet_type(&buf), Some(u16::from_be_bytes([buf[0], buf[1]])));
        } else {
            prop_assert_eq!(ControlPacket::packet_type(&buf), None);
        }
    }

    // ---- ROUND-TRIP: parse(build(x)) == x for well-formed packets ----

    /// Extended keepalive: `Keepalive::conn_info(Keepalive::encode_ext(info, ..)) == info`.
    #[test]
    fn keepalive_ext_roundtrips(info in arb_conn_info()) {
        let pkt = Keepalive::encode_ext(info, 1_000_000);
        prop_assert_eq!(ControlPacket::packet_type(&pkt), Some(srtla_protocol::SRTLA_TYPE_KEEPALIVE));
        prop_assert!(Keepalive::timestamp(&pkt).is_some());
        prop_assert_eq!(Keepalive::conn_info(&pkt), Some(info));
    }

    /// SRTLA ACK: `SrtlaAck::parse(SrtlaAck::encode(acks)) == acks`.
    #[test]
    fn srtla_ack_roundtrips(acks in prop::collection::vec(any::<u32>(), 0..64)) {
        let pkt = SrtlaAck::encode(&acks);
        let parsed = SrtlaAck::parse(&pkt);
        prop_assert_eq!(parsed.as_slice(), acks.as_slice());
    }

    /// SRT NAK, single-loss list: a frame whose words all have the high bit
    /// clear decodes back to exactly those sequence numbers.
    #[test]
    fn srt_nak_singles_roundtrip(seqs in prop::collection::vec(0u32..0x8000_0000, 0..64)) {
        let mut buf = nak_header();
        for &s in &seqs {
            buf.extend_from_slice(&s.to_be_bytes());
        }
        let parsed = ControlPacket::parse_srt_nak(&buf);
        prop_assert_eq!(parsed.as_slice(), seqs.as_slice());
    }

    /// SRT NAK, single range: a high-bit-set start word followed by an end word
    /// expands to the inclusive `start..=end` sequence (delta kept small so the
    /// expansion stays well under the parser's 1000-entry cap).
    #[test]
    fn srt_nak_range_roundtrips(start in 0u32..0x7fff_0000, delta in 0u32..200) {
        let end = start + delta;
        let mut buf = nak_header();
        buf.extend_from_slice(&(start | 0x8000_0000).to_be_bytes());
        buf.extend_from_slice(&end.to_be_bytes());
        let parsed = ControlPacket::parse_srt_nak(&buf);
        let expected: Vec<u32> = (start..=end).collect();
        prop_assert_eq!(parsed.as_slice(), expected.as_slice());
    }

    /// SRT ACK: a well-formed 20-byte ACK frame round-trips its ack number.
    #[test]
    fn srt_ack_roundtrips(ack in any::<u32>()) {
        let mut buf = vec![0u8; 20];
        buf[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
        buf[16..20].copy_from_slice(&ack.to_be_bytes());
        prop_assert!(ControlPacket::is_srt_ack(&buf));
        prop_assert_eq!(ControlPacket::parse_srt_ack(&buf), Some(ack));
    }

    /// REG1 / REG2: the builders produce frames the type validators accept and
    /// whose embedded id round-trips byte-for-byte.
    #[test]
    fn reg1_reg2_roundtrip(id in prop::collection::vec(any::<u8>(), SRTLA_ID_LEN..=SRTLA_ID_LEN)) {
        let id: [u8; SRTLA_ID_LEN] = id.try_into().expect("length pinned to SRTLA_ID_LEN");

        let r1 = RegPacket::reg1(&id);
        prop_assert!(RegPacket::is_reg1(&r1));
        prop_assert_eq!(&r1[2..], &id[..]);

        let r2 = RegPacket::reg2(&id);
        prop_assert!(RegPacket::is_reg2(&r2));
        prop_assert_eq!(&r2[2..], &id[..]);
    }

    /// Standard keepalive: builder output carries a recoverable timestamp and
    /// is detected as a keepalive, but is NOT an extended-info frame.
    #[test]
    fn standard_keepalive_has_timestamp_no_conn_info(_ in 0u8..1) {
        let pkt = Keepalive::encode(1_000_000);
        prop_assert!(Keepalive::is_keepalive(&pkt));
        prop_assert!(Keepalive::timestamp(&pkt).is_some());
        prop_assert!(Keepalive::conn_info(&pkt).is_none());
    }
}
