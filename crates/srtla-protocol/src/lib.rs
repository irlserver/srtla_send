//! SRTLA wire protocol: pure, stateless packet (de)serialization.
//!
//! No clock, no state, no I/O — just byte layout for the SRTLA registration
//! handshake, SRT ACK/NAK, and keepalives. The sender crate re-exports this as
//! `crate::protocol`, so `use crate::protocol::*` keeps working there.

mod builders;
mod constants;
mod parsers;
mod types;

// Re-export the full public surface at the crate root.

// Constants
// Builders
#[allow(unused_imports)]
pub use builders::{
    create_ack_packet, create_keepalive_packet, create_keepalive_packet_ext, create_reg1_packet,
    create_reg2_packet,
};
pub use constants::*;
// Parsers
#[allow(unused_imports)]
pub use parsers::{
    extract_keepalive_conn_info, extract_keepalive_timestamp, parse_srt_ack,
    parse_srt_handshake_latency, parse_srt_nak, parse_srtla_ack,
};
// Types and helpers
#[allow(unused_imports)]
pub use types::{
    ConnectionInfo, SrtHandshakeLatency, get_packet_type, get_srt_sequence_number, is_srt_ack,
    is_srt_data_retransmit, is_srtla_keepalive, is_srtla_reg1, is_srtla_reg2, is_srtla_reg3,
    set_srt_data_retransmit,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extended_keepalive_roundtrip() {
        let info = ConnectionInfo {
            conn_id: 42,
            window: 25000,
            in_flight: 8,
            rtt_ms: 120,
            nak_count: 5,
            bitrate_bytes_per_sec: 2_500_000,
        };

        let pkt = create_keepalive_packet_ext(info, 123_456);

        // Verify packet length
        assert_eq!(pkt.len(), SRTLA_KEEPALIVE_EXT_LEN);

        // Verify packet type
        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_KEEPALIVE));

        // Verify timestamp extraction works (backwards compatible)
        assert!(extract_keepalive_timestamp(&pkt).is_some());

        // Verify connection info extraction
        let extracted = extract_keepalive_conn_info(&pkt).unwrap();
        assert_eq!(extracted, info);
    }

    #[test]
    fn test_standard_keepalive_no_conn_info() {
        let pkt = create_keepalive_packet(123_456);

        // Standard keepalive should not have connection info
        assert_eq!(pkt.len(), 10);
        assert!(extract_keepalive_timestamp(&pkt).is_some());
        assert!(extract_keepalive_conn_info(&pkt).is_none());
    }

    #[test]
    fn test_extended_keepalive_backwards_compat() {
        let info = ConnectionInfo {
            conn_id: 1,
            window: 20000,
            in_flight: 5,
            rtt_ms: 100,
            nak_count: 2,
            bitrate_bytes_per_sec: 1_000_000,
        };

        // Same injected timestamp on both so the ext/std comparison below is
        // exact (this crate no longer reads a clock).
        let ext_pkt = create_keepalive_packet_ext(info, 123_456);

        // Old receiver behavior: only reads first 10 bytes
        let timestamp_from_ext = extract_keepalive_timestamp(&ext_pkt);
        assert!(timestamp_from_ext.is_some());

        // Compare with standard keepalive timestamp
        let std_pkt = create_keepalive_packet(123_456);
        let timestamp_from_std = extract_keepalive_timestamp(&std_pkt);
        assert!(timestamp_from_std.is_some());

        // Both should be valid timestamps (within 1 second of each other)
        let diff = timestamp_from_ext
            .unwrap()
            .abs_diff(timestamp_from_std.unwrap());
        assert!(diff < 1000); // Less than 1 second difference
    }

    #[test]
    fn test_retransmit_flag_detection() {
        // SRT data packet: MSB of the sequence word clear; second word is
        // PP(2) O(1) KK(2) R(1) msg-number(26) — R is bit 26, i.e. 0x04 in
        // byte 4.
        let mut pkt = [0u8; 16];
        pkt[0..4].copy_from_slice(&100u32.to_be_bytes());
        assert!(!is_srt_data_retransmit(&pkt), "original send has R clear");

        pkt[4] |= 0x04;
        assert!(is_srt_data_retransmit(&pkt), "R bit marks a retransmission");

        // PP/O/KK bits alone must not read as a retransmit.
        let mut flags = [0u8; 16];
        flags[0..4].copy_from_slice(&100u32.to_be_bytes());
        flags[4] = 0xf8; // PP=11 O=1 KK=11, R=0
        assert!(!is_srt_data_retransmit(&flags));

        // Setting the bit is the inverse of reading it, and touches nothing
        // else in the header — the sequence number in particular, which is
        // what the receiver dedups a probe copy by.
        let mut probe = [0u8; 16];
        probe[0..4].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        probe[4..8].copy_from_slice(&0x0abc_defau32.to_be_bytes());
        let before = probe;
        set_srt_data_retransmit(&mut probe);
        assert!(is_srt_data_retransmit(&probe), "R bit must be set");
        assert_eq!(probe[0..4], before[0..4], "sequence number must not move");
        assert_eq!(
            probe[4] & !0x04,
            before[4] & !0x04,
            "no other flag in the second word may change"
        );
        assert_eq!(probe[5..], before[5..], "message number must not move");
        // Idempotent: a copy of an already-retransmitted packet stays valid.
        set_srt_data_retransmit(&mut probe);
        assert!(is_srt_data_retransmit(&probe));

        // Control packets and runt buffers are left alone.
        let mut ctrl = [0u8; 16];
        ctrl[0] = 0x80;
        let ctrl_before = ctrl;
        set_srt_data_retransmit(&mut ctrl);
        assert_eq!(ctrl, ctrl_before, "control packets must not be touched");
        let mut runt = [0u8; 4];
        set_srt_data_retransmit(&mut runt);
        assert_eq!(runt, [0u8; 4], "a runt buffer must not be indexed into");

        // Control packets (MSB set) are never retransmits.
        let mut ctrl = [0u8; 16];
        ctrl[0] = 0x80;
        ctrl[4] = 0x04;
        assert!(!is_srt_data_retransmit(&ctrl));

        // Truncated buffers are rejected.
        assert!(!is_srt_data_retransmit(&pkt[..7]));
    }

    #[test]
    fn test_extended_keepalive_wrong_magic() {
        let mut pkt = [0u8; SRTLA_KEEPALIVE_EXT_LEN];
        pkt[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
        pkt[10..12].copy_from_slice(&0xdeadu16.to_be_bytes()); // Wrong magic

        assert!(extract_keepalive_conn_info(&pkt).is_none());
    }

    #[test]
    fn test_extended_keepalive_wrong_version() {
        let mut pkt = [0u8; SRTLA_KEEPALIVE_EXT_LEN];
        pkt[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
        pkt[10..12].copy_from_slice(&SRTLA_KEEPALIVE_MAGIC.to_be_bytes());
        pkt[12..14].copy_from_slice(&0x9999u16.to_be_bytes()); // Wrong version

        assert!(extract_keepalive_conn_info(&pkt).is_none());
    }

    /// `SRT_CMD_SID`, a variable-length block libsrt writes before HSREQ when a
    /// stream ID is configured. Used here only to push HSREQ off the front.
    const SRT_HS_EXT_CMD_SID: u16 = 5;

    /// Assemble an SRT handshake with the given version, request type,
    /// extension field and extension blocks (`(command, body words)`).
    fn handshake(
        version: u32,
        reqtype: i32,
        ext_field: u32,
        blocks: &[(u16, Vec<u32>)],
    ) -> Vec<u8> {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&SRT_TYPE_HANDSHAKE.to_be_bytes());
        pkt.extend_from_slice(&[0u8; SRT_CONTROL_HEADER_LEN - 2]);

        let mut cif = [0u32; SRT_HANDSHAKE_CIF_LEN / 4];
        cif[0] = version;
        cif[1] = ext_field;
        cif[5] = reqtype as u32;
        for word in cif {
            pkt.extend_from_slice(&word.to_be_bytes());
        }

        for (cmd, body) in blocks {
            let spec = ((*cmd as u32) << 16) | body.len() as u32;
            pkt.extend_from_slice(&spec.to_be_bytes());
            for word in body {
                pkt.extend_from_slice(&word.to_be_bytes());
            }
        }
        pkt
    }

    /// An HSREQ/HSRSP body: version, flags, `rcv | snd` latency.
    fn hs_body(flags: u32, rcv_ms: u16, snd_ms: u16) -> Vec<u32> {
        vec![
            0x0001_0500, // SRT 1.5.0
            flags,
            ((rcv_ms as u32) << 16) | snd_ms as u32,
        ]
    }

    fn conclusion(blocks: &[(u16, Vec<u32>)]) -> Vec<u8> {
        handshake(
            SRT_HS_VERSION_5,
            SRT_HS_REQTYPE_CONCLUSION,
            SRT_HS_EXT_FLAG_HSREQ,
            blocks,
        )
    }

    const BOTH_TSBPD: u32 = SRT_HS_OPT_TSBPDSND | SRT_HS_OPT_TSBPDRCV;

    #[test]
    fn hsrsp_yields_the_far_ends_receive_latency() {
        // The responder answers with its own (already negotiated) receive delay
        // in the high half. That 4000 ms is the deadline our packets must beat.
        let pkt = conclusion(&[(SRT_HS_EXT_CMD_HSRSP, hs_body(BOTH_TSBPD, 4000, 120))]);
        let hs = parse_srt_handshake_latency(&pkt).expect("a conclusion HSRSP must parse");
        assert!(hs.is_response);
        assert_eq!(hs.rcv_ms, Some(4000));
        assert_eq!(hs.snd_ms, Some(120));
    }

    #[test]
    fn hsreq_is_marked_as_a_proposal_not_a_response() {
        // Same bytes, HSREQ command: the initiator's ask, before negotiation.
        let pkt = conclusion(&[(SRT_HS_EXT_CMD_HSREQ, hs_body(BOTH_TSBPD, 4000, 120))]);
        let hs = parse_srt_handshake_latency(&pkt).unwrap();
        assert!(!hs.is_response, "HSREQ is not the negotiated answer");
        assert_eq!(hs.rcv_ms, Some(4000));
    }

    #[test]
    fn each_latency_half_is_gated_on_its_own_tsbpd_flag() {
        // With TSBPD off the 16 bits are undefined, not zero — reporting 0 ms
        // would look like an impossibly tight budget rather than "unknown".
        let none = conclusion(&[(SRT_HS_EXT_CMD_HSRSP, hs_body(0, 4000, 120))]);
        let hs = parse_srt_handshake_latency(&none).unwrap();
        assert_eq!(hs.rcv_ms, None);
        assert_eq!(hs.snd_ms, None);

        let rcv_only = conclusion(&[(
            SRT_HS_EXT_CMD_HSRSP,
            hs_body(SRT_HS_OPT_TSBPDRCV, 4000, 120),
        )]);
        let hs = parse_srt_handshake_latency(&rcv_only).unwrap();
        assert_eq!(hs.rcv_ms, Some(4000));
        assert_eq!(hs.snd_ms, None);
    }

    #[test]
    fn hsrsp_is_found_behind_earlier_blocks() {
        // libsrt writes SID, key material and others around HSREQ/HSRSP, so the
        // walk cannot assume it comes first. The zero-length block also proves
        // the walk still advances when a body is empty.
        let pkt = conclusion(&[
            (SRT_HS_EXT_CMD_SID, vec![0x7465_7374, 0x0000_0000]),
            (99, vec![]),
            (SRT_HS_EXT_CMD_HSRSP, hs_body(BOTH_TSBPD, 2500, 80)),
        ]);
        let hs = parse_srt_handshake_latency(&pkt).unwrap();
        assert_eq!(hs.rcv_ms, Some(2500));
    }

    #[test]
    fn only_an_hsv5_conclusion_is_read() {
        let body = [(SRT_HS_EXT_CMD_HSRSP, hs_body(BOTH_TSBPD, 4000, 120))];

        // Induction: word 1 holds a magic cookie, not extension flags.
        let induction = handshake(SRT_HS_VERSION_5, 1, SRT_HS_EXT_FLAG_HSREQ, &body);
        assert!(parse_srt_handshake_latency(&induction).is_none());

        // HSv4 keeps its SRT handshake in a separate control packet.
        let v4 = handshake(4, SRT_HS_REQTYPE_CONCLUSION, SRT_HS_EXT_FLAG_HSREQ, &body);
        assert!(parse_srt_handshake_latency(&v4).is_none());

        // Rejection codes are >= 1000.
        let rejected = handshake(SRT_HS_VERSION_5, 1002, SRT_HS_EXT_FLAG_HSREQ, &body);
        assert!(parse_srt_handshake_latency(&rejected).is_none());

        // Extension field says there is no HSREQ block to look for.
        let no_ext = handshake(SRT_HS_VERSION_5, SRT_HS_REQTYPE_CONCLUSION, 0, &body);
        assert!(parse_srt_handshake_latency(&no_ext).is_none());

        // Not a handshake at all.
        let mut ack = vec![0u8; 64];
        ack[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
        assert!(parse_srt_handshake_latency(&ack).is_none());
    }

    #[test]
    fn a_block_length_running_past_the_packet_is_rejected() {
        let mut pkt = conclusion(&[(SRT_HS_EXT_CMD_HSRSP, hs_body(BOTH_TSBPD, 4000, 120))]);
        // Claim 0xFFFF words of body where only 3 follow.
        let spec = SRT_CONTROL_HEADER_LEN + SRT_HANDSHAKE_CIF_LEN;
        pkt[spec + 2..spec + 4].copy_from_slice(&0xffffu16.to_be_bytes());
        assert!(parse_srt_handshake_latency(&pkt).is_none());
    }

    #[test]
    fn a_short_hsrsp_block_is_rejected() {
        // Two words where three are required: the latency word is absent.
        let pkt = conclusion(&[(SRT_HS_EXT_CMD_HSRSP, vec![0x0001_0500, BOTH_TSBPD])]);
        assert!(parse_srt_handshake_latency(&pkt).is_none());
    }

    #[test]
    fn no_prefix_of_a_handshake_panics() {
        // Every one of these bytes comes off the network from the far end, so
        // the walk has to be total: no indexing panic, no runaway loop.
        let pkt = conclusion(&[
            (SRT_HS_EXT_CMD_SID, vec![0x7465_7374]),
            (SRT_HS_EXT_CMD_HSRSP, hs_body(BOTH_TSBPD, 4000, 120)),
        ]);
        for len in 0..=pkt.len() {
            let _ = parse_srt_handshake_latency(&pkt[..len]);
        }
        assert_eq!(
            parse_srt_handshake_latency(&pkt).unwrap().rcv_ms,
            Some(4000),
            "the intact packet must still parse"
        );
    }

    #[test]
    fn a_garbage_extension_area_terminates() {
        // Spec words drawn to look nothing like real blocks. Each step consumes
        // at least the 4-byte spec word, so the walk must end either way.
        let mut pkt = conclusion(&[]);
        for i in 0..64u32 {
            pkt.extend_from_slice(&(0xdead_0000u32.wrapping_add(i)).to_be_bytes());
        }
        let _ = parse_srt_handshake_latency(&pkt);
    }
}
