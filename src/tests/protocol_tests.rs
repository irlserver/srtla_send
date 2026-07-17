#[cfg(test)]
mod tests {
    // Protocol invariant assertions over compile-time constants document the
    // contract rather than test runtime values.
    #![allow(clippy::assertions_on_constants)]

    use crate::protocol::*;

    #[test]
    fn test_get_packet_type() {
        // Valid packet types
        let buf = [0x90, 0x00, 0x01, 0x02];
        assert_eq!(get_packet_type(&buf), Some(SRTLA_TYPE_KEEPALIVE));

        let buf = [0x80, 0x02, 0x01, 0x02];
        assert_eq!(get_packet_type(&buf), Some(SRT_TYPE_ACK));

        // Empty buffer
        assert_eq!(get_packet_type(&[]), None);

        // Buffer too short
        assert_eq!(get_packet_type(&[0x90]), None);
    }

    #[test]
    fn test_get_srt_sequence_number() {
        // Valid sequence number (control bit not set)
        let buf = [0x00, 0x00, 0x10, 0x00];
        assert_eq!(get_srt_sequence_number(&buf), Some(0x1000));

        // Invalid sequence number (control bit set)
        let buf = [0x80, 0x00, 0x10, 0x00];
        assert_eq!(get_srt_sequence_number(&buf), None);

        // Valid sequence number (sequence 0)
        let buf = [0x00, 0x00, 0x00, 0x00];
        assert_eq!(get_srt_sequence_number(&buf), Some(0x0000));

        // Buffer too short
        assert_eq!(get_srt_sequence_number(&[0x00, 0x00]), None);

        // Empty buffer
        assert_eq!(get_srt_sequence_number(&[]), None);
    }

    #[test]
    fn test_create_reg1_packet() {
        let id = [0x42; SRTLA_ID_LEN];
        let pkt = create_reg1_packet(&id);

        assert_eq!(pkt.len(), SRTLA_TYPE_REG1_LEN);
        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_REG1));
        assert!(pkt[2..].iter().all(|&b| b == 0x42));
        assert!(is_srtla_reg1(&pkt));
    }

    #[test]
    fn test_create_reg2_packet() {
        let id = [0x24; SRTLA_ID_LEN];
        let pkt = create_reg2_packet(&id);

        assert_eq!(pkt.len(), SRTLA_TYPE_REG2_LEN);
        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_REG2));
        assert!(pkt[2..].iter().all(|&b| b == 0x24));
        assert!(is_srtla_reg2(&pkt));
    }

    #[test]
    fn test_create_keepalive_packet() {
        let pkt = create_keepalive_packet();

        assert_eq!(pkt.len(), 10);
        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_KEEPALIVE));
        assert!(is_srtla_keepalive(&pkt));

        // Test timestamp extraction
        let timestamp = extract_keepalive_timestamp(&pkt).unwrap();
        assert!(timestamp > 0);
    }

    #[test]
    fn test_extract_keepalive_timestamp() {
        // Create a keepalive packet with known timestamp
        let mut pkt = vec![0u8; 10];
        pkt[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
        let test_ts = 0x0102030405060708u64;
        for i in 0..8 {
            pkt[2 + i] = ((test_ts >> (56 - i * 8)) & 0xff) as u8;
        }

        assert_eq!(extract_keepalive_timestamp(&pkt), Some(test_ts));

        // Test with invalid packet type
        pkt[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
        assert_eq!(extract_keepalive_timestamp(&pkt), None);

        // Test with buffer too short
        assert_eq!(extract_keepalive_timestamp(&pkt[..5]), None);
    }

    #[test]
    fn test_create_ack_packet() {
        let acks = [100, 200, 300];
        let pkt = create_ack_packet(&acks);

        assert_eq!(pkt.len(), 4 + 4 * acks.len());
        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_ACK));

        // Note: This test validates packet creation, not parsing compatibility
    }

    #[test]
    fn test_create_ack_packet_with_4byte_header() {
        let acks = [100u32, 200, 300];
        let mut pkt = vec![0u8; 4 + 4 * acks.len()];

        pkt[0..2].copy_from_slice(&SRTLA_TYPE_ACK.to_be_bytes());
        pkt[2] = 0x00;
        pkt[3] = 0x00;

        for (i, &ack) in acks.iter().enumerate() {
            let off = 4 + i * 4;
            pkt[off..off + 4].copy_from_slice(&ack.to_be_bytes());
        }

        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_ACK));
        let parsed_acks = parse_srtla_ack(&pkt);
        assert_eq!(parsed_acks.as_slice(), &[100, 200, 300]);
    }

    #[test]
    fn test_parse_srt_ack() {
        let mut buf = vec![0u8; 20];
        buf[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
        buf[16..20].copy_from_slice(&12345u32.to_be_bytes());

        assert_eq!(parse_srt_ack(&buf), Some(12345));

        // Test with wrong packet type
        buf[0] = 0x90;
        assert_eq!(parse_srt_ack(&buf), None);

        // Test with buffer too short
        assert_eq!(parse_srt_ack(&buf[..19]), None);
    }

    #[test]
    fn test_parse_srt_nak_single() {
        let mut buf = vec![0u8; 8];
        buf[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());
        buf[4..8].copy_from_slice(&500u32.to_be_bytes());

        let naks = parse_srt_nak(&buf);
        assert_eq!(naks.as_slice(), &[500]);
    }

    #[test]
    fn test_parse_srt_nak_range() {
        let mut buf = vec![0u8; 12];
        buf[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());
        // Range NAK: set high bit and provide start/end
        let start = 100u32 | 0x8000_0000;
        buf[4..8].copy_from_slice(&start.to_be_bytes());
        buf[8..12].copy_from_slice(&103u32.to_be_bytes());

        let naks = parse_srt_nak(&buf);
        assert_eq!(naks.as_slice(), &[100, 101, 102, 103]);
    }

    #[test]
    fn test_parse_srt_nak_mixed() {
        let mut buf = vec![0u8; 16];
        buf[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());

        // First: single NAK
        buf[4..8].copy_from_slice(&50u32.to_be_bytes());

        // Second: range NAK
        let start = 100u32 | 0x8000_0000;
        buf[8..12].copy_from_slice(&start.to_be_bytes());
        buf[12..16].copy_from_slice(&102u32.to_be_bytes());

        let naks = parse_srt_nak(&buf);
        assert_eq!(naks.as_slice(), &[50, 100, 101, 102]);
    }

    #[test]
    fn test_parse_srt_nak_invalid() {
        // Wrong packet type
        let mut buf = vec![0u8; 8];
        buf[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
        assert!(parse_srt_nak(&buf).is_empty());

        // Buffer too short
        assert!(parse_srt_nak(&[0x80, 0x03, 0x00]).is_empty());
    }

    #[test]
    fn test_parse_srtla_ack() {
        // Test with 4-byte header format
        let acks = [1000u32, 2000, 3000];
        let mut pkt = vec![0u8; 4 + 4 * acks.len()];

        pkt[0..2].copy_from_slice(&SRTLA_TYPE_ACK.to_be_bytes());
        pkt[2] = 0x00;
        pkt[3] = 0x00;

        for (i, &ack) in acks.iter().enumerate() {
            let off = 4 + i * 4;
            pkt[off..off + 4].copy_from_slice(&ack.to_be_bytes());
        }

        let parsed = parse_srtla_ack(&pkt);
        assert_eq!(parsed.as_slice(), &[1000, 2000, 3000]);

        // Test empty ACK packet (4-byte header, no sequences)
        let empty_pkt = vec![0x91, 0x00, 0x00, 0x00];
        let parsed_empty = parse_srtla_ack(&empty_pkt);
        assert_eq!(parsed_empty.as_slice(), &[] as &[u32]);

        // Test invalid packet type
        let mut invalid = pkt.clone();
        invalid[0] = 0x80;
        assert!(parse_srtla_ack(&invalid).is_empty());

        // Test buffer too short (less than 8 bytes for 4-byte header format)
        assert!(parse_srtla_ack(&[0x91, 0x00, 0x00]).is_empty());
    }

    #[test]
    fn test_packet_type_validators() {
        let reg1_id = [0x11; SRTLA_ID_LEN];
        let reg1_pkt = create_reg1_packet(&reg1_id);
        assert!(is_srtla_reg1(&reg1_pkt));
        assert!(!is_srtla_reg2(&reg1_pkt));
        assert!(!is_srtla_reg3(&reg1_pkt));

        let reg2_id = [0x22; SRTLA_ID_LEN];
        let reg2_pkt = create_reg2_packet(&reg2_id);
        assert!(!is_srtla_reg1(&reg2_pkt));
        assert!(is_srtla_reg2(&reg2_pkt));
        assert!(!is_srtla_reg3(&reg2_pkt));

        let reg3_pkt = vec![(SRTLA_TYPE_REG3 >> 8) as u8, (SRTLA_TYPE_REG3 & 0xff) as u8];
        assert!(!is_srtla_reg1(&reg3_pkt));
        assert!(!is_srtla_reg2(&reg3_pkt));
        assert!(is_srtla_reg3(&reg3_pkt));

        let keepalive_pkt = create_keepalive_packet();
        assert!(is_srtla_keepalive(&keepalive_pkt));

        let mut ack_pkt = vec![0u8; 20];
        ack_pkt[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
        assert!(is_srt_ack(&ack_pkt));
    }

    #[test]
    fn test_constants() {
        assert_eq!(SRTLA_ID_LEN, 256);
        assert_eq!(SRTLA_TYPE_REG1_LEN, 2 + SRTLA_ID_LEN);
        assert_eq!(SRTLA_TYPE_REG2_LEN, 2 + SRTLA_ID_LEN);
        assert_eq!(SRTLA_TYPE_REG3_LEN, 2);

        assert!(WINDOW_MIN < WINDOW_DEF);
        assert!(WINDOW_DEF < WINDOW_MAX);
        assert!(WINDOW_INCR > 0);
        assert!(WINDOW_DECR > 0);
        assert!(WINDOW_MULT > 0);
    }
}

// Frozen on-wire byte pins for the registration handshake (REG1/REG2 = 258 B,
// REG3 = 2 B) and the bare keepalive (2 B). A failure here means a layout or
// constant drifted and wire compatibility with the receiver broke.
// Top-level module so `cargo test protocol_tests::encode` selects exactly this group.
#[cfg(test)]
mod encode {
    use crate::protocol::*;

    #[test]
    fn reg1_first_two_bytes_and_total_len() {
        let id = [0xabu8; SRTLA_ID_LEN];
        let buf = create_reg1_packet(&id);

        assert_eq!(&buf[0..2], &[0x92u8, 0x00], "REG1 type must be 0x9200 BE");
        assert_eq!(buf.len(), 258, "REG1 frame is exactly 258 bytes");
    }

    #[test]
    fn reg2_first_two_bytes_and_total_len() {
        let id = [0xcdu8; SRTLA_ID_LEN];
        let buf = create_reg2_packet(&id);

        assert_eq!(&buf[0..2], &[0x92u8, 0x01], "REG2 type must be 0x9201 BE");
        assert_eq!(buf.len(), 258, "REG2 frame is exactly 258 bytes");
    }

    #[test]
    fn reg3_type_and_len() {
        // REG3 has no builder: the receiver emits the bare 2-byte type frame and
        // the sender echo-handles it. Pin its wire form from the frozen constant.
        let buf = SRTLA_TYPE_REG3.to_be_bytes();

        assert_eq!(&buf[..], &[0x92u8, 0x02], "REG3 type must be 0x9202 BE");
        assert_eq!(buf.len(), 2, "REG3 frame is exactly 2 bytes");
    }

    #[test]
    fn keepalive_is_bare_2_bytes() {
        // Caveat that prevents a false "fix": the live send_keepalive() emits the
        // backwards-compatible extended 38-byte keepalive, not this bare form.
        // This pins the minimal 2-byte keepalive the protocol still guarantees;
        // it does not assert which form the sender emits.
        let buf = SRTLA_TYPE_KEEPALIVE.to_be_bytes();

        assert_eq!(
            &buf[..],
            &[0x90u8, 0x00],
            "bare KEEPALIVE type must be 0x9000 BE"
        );
        assert_eq!(buf.len(), 2, "bare KEEPALIVE frame is exactly 2 bytes");
        assert!(
            !buf.windows(2)
                .any(|w| w == SRTLA_KEEPALIVE_MAGIC.to_be_bytes()),
            "bare KEEPALIVE must not contain the 0xC01F extended magic"
        );
    }
}

// REG3/REG_ERR/REG_NGP arrive as bare 2-byte type frames (the receiver's
// pad_sendto 32 B padding is ignored), so the sender's "decode" of them is the
// get_packet_type discriminator plus the length-checked is_srtla_* validators.
#[cfg(test)]
mod decode {
    use crate::protocol::*;

    #[test]
    fn decode_reg2_valid() {
        let id = [0x5au8; SRTLA_ID_LEN];
        let pkt = create_reg2_packet(&id);

        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_REG2));
        assert!(is_srtla_reg2(&pkt));
        assert!(!is_srtla_reg1(&pkt));
        assert!(!is_srtla_reg3(&pkt));
        assert_eq!(&pkt[2..], &id[..]);
    }

    #[test]
    fn decode_reg3_valid() {
        let pkt = SRTLA_TYPE_REG3.to_be_bytes();

        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_REG3));
        assert!(is_srtla_reg3(&pkt));
        assert!(!is_srtla_reg1(&pkt));
        assert!(!is_srtla_reg2(&pkt));
    }

    #[test]
    fn decode_reg_err_valid() {
        let pkt = SRTLA_TYPE_REG_ERR.to_be_bytes();

        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_REG_ERR));
        assert!(!is_srtla_reg1(&pkt));
        assert!(!is_srtla_reg2(&pkt));
        assert!(!is_srtla_reg3(&pkt));
    }

    #[test]
    fn decode_reg_ngp_valid() {
        let pkt = SRTLA_TYPE_REG_NGP.to_be_bytes();

        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_REG_NGP));
        assert!(!is_srtla_reg1(&pkt));
        assert!(!is_srtla_reg2(&pkt));
        assert!(!is_srtla_reg3(&pkt));
    }

    #[test]
    fn decode_ack_valid() {
        let acks = [1234u32, 5678, 9012];
        let pkt = create_ack_packet(&acks);

        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_ACK));
        let parsed = parse_srtla_ack(&pkt);
        assert_eq!(parsed.as_slice(), &acks[..]);
    }

    #[test]
    fn decode_srt_ack_nak() {
        let mut ack = vec![0u8; 20];
        ack[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
        ack[16..20].copy_from_slice(&424_242u32.to_be_bytes()); // ack seq at bytes 16..20

        assert_eq!(get_packet_type(&ack), Some(SRT_TYPE_ACK));
        assert!(is_srt_ack(&ack));
        assert_eq!(parse_srt_ack(&ack), Some(424_242));

        let mut nak = vec![0u8; 8];
        nak[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());
        nak[4..8].copy_from_slice(&777u32.to_be_bytes()); // single lost seq at bytes 4..8

        assert_eq!(get_packet_type(&nak), Some(SRT_TYPE_NAK));
        let parsed = parse_srt_nak(&nak);
        assert_eq!(parsed.as_slice(), &[777]);
    }

    #[test]
    fn decode_keepalive_valid() {
        let pkt = create_keepalive_packet();

        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_KEEPALIVE));
        assert!(is_srtla_keepalive(&pkt));
        assert!(extract_keepalive_timestamp(&pkt).is_some());
    }
}

// Pins the graceful-rejection contract: every degenerate input returns
// None/empty, never panics. The parsers return Option/SmallVec by design, so
// these tests guard against an upstream merge regressing that into an unwrap.
#[cfg(test)]
mod malformed {
    use crate::protocol::*;

    #[test]
    fn zero_length_returns_none_or_err() {
        let empty: &[u8] = &[];

        assert_eq!(get_packet_type(empty), None);
        assert_eq!(get_srt_sequence_number(empty), None);
        assert_eq!(parse_srt_ack(empty), None);
        assert_eq!(extract_keepalive_timestamp(empty), None);
        assert!(extract_keepalive_conn_info(empty).is_none());
        assert!(parse_srt_nak(empty).is_empty());
        assert!(parse_srtla_ack(empty).is_empty());
    }

    #[test]
    fn truncated_id_returns_none_or_err() {
        let mut buf = vec![0u8; 2 + SRTLA_ID_LEN / 2];
        buf[0..2].copy_from_slice(&SRTLA_TYPE_REG2.to_be_bytes());

        // Length-checked validator rejects the half-length id, yet the bare
        // 2-byte type still reads cleanly without panicking.
        assert!(!is_srtla_reg2(&buf));
        assert!(!is_srtla_reg1(&buf));
        assert_eq!(get_packet_type(&buf), Some(SRTLA_TYPE_REG2));
    }

    #[test]
    fn unknown_type_returns_none_or_err() {
        let mut buf = vec![0u8; 20];
        buf[0..2].copy_from_slice(&0x9999u16.to_be_bytes());

        assert_eq!(parse_srt_ack(&buf), None);
        assert_eq!(extract_keepalive_timestamp(&buf), None);
        assert!(extract_keepalive_conn_info(&buf).is_none());
        assert!(parse_srt_nak(&buf).is_empty());
        assert!(parse_srtla_ack(&buf).is_empty());
        assert!(!is_srtla_reg1(&buf));
        assert!(!is_srtla_reg2(&buf));
        assert!(!is_srtla_reg3(&buf));
        assert!(!is_srtla_keepalive(&buf));
        assert!(!is_srt_ack(&buf));
    }

    #[test]
    fn short_frame_returns_none_or_err() {
        let one = [0x91u8];

        assert_eq!(get_packet_type(&one), None);
        assert_eq!(get_srt_sequence_number(&one), None);
        assert_eq!(parse_srt_ack(&one), None);
        assert_eq!(extract_keepalive_timestamp(&one), None);
        assert!(extract_keepalive_conn_info(&one).is_none());
        assert!(parse_srt_nak(&one).is_empty());
        assert!(parse_srtla_ack(&one).is_empty());
    }
}
