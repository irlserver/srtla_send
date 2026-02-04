#[cfg(test)]
mod tests {
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
