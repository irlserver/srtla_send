#![cfg(test)]

use crate::protocol::*;

#[tokio::test]
async fn test_protocol_packet_roundtrip() {
    // Test REG1 packet creation and validation
    let id = [0x42; SRTLA_ID_LEN];
    let reg1_packet = create_reg1_packet(&id);

    assert!(is_srtla_reg1(&reg1_packet));
    assert_eq!(get_packet_type(&reg1_packet), Some(SRTLA_TYPE_REG1));

    // Test REG2 packet creation and validation
    let reg2_packet = create_reg2_packet(&id);
    assert!(is_srtla_reg2(&reg2_packet));
    assert_eq!(get_packet_type(&reg2_packet), Some(SRTLA_TYPE_REG2));

    // Test keepalive packet creation and timestamp extraction
    let keepalive_packet = create_keepalive_packet();
    assert!(is_srtla_keepalive(&keepalive_packet));
    assert_eq!(
        get_packet_type(&keepalive_packet),
        Some(SRTLA_TYPE_KEEPALIVE)
    );

    let timestamp = extract_keepalive_timestamp(&keepalive_packet);
    assert!(timestamp.is_some());
    assert!(timestamp.unwrap() > 0);
}

#[tokio::test]
async fn test_srt_ack_nak_parsing() {
    // Test SRT ACK packet parsing
    let mut ack_packet = vec![0u8; 20];
    ack_packet[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
    ack_packet[16..20].copy_from_slice(&54321u32.to_be_bytes());

    assert_eq!(parse_srt_ack(&ack_packet), Some(54321));
    assert!(is_srt_ack(&ack_packet));

    // Test SRT NAK packet parsing - single NAK
    let mut nak_packet = vec![0u8; 8];
    nak_packet[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());
    nak_packet[4..8].copy_from_slice(&12345u32.to_be_bytes());

    let naks = parse_srt_nak(&nak_packet);
    assert_eq!(naks, vec![12345]);

    // Test SRT NAK packet parsing - range NAK
    let mut range_nak_packet = vec![0u8; 12];
    range_nak_packet[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());
    let range_start = 1000u32 | 0x8000_0000;
    range_nak_packet[4..8].copy_from_slice(&range_start.to_be_bytes());
    range_nak_packet[8..12].copy_from_slice(&1003u32.to_be_bytes());

    let range_naks = parse_srt_nak(&range_nak_packet);
    assert_eq!(range_naks, vec![1000, 1001, 1002, 1003]);
}

#[tokio::test]
async fn test_srtla_ack_roundtrip() {
    let original_acks = vec![100u32, 200, 300, 400];
    let ack_packet = create_ack_packet(&original_acks);

    assert_eq!(get_packet_type(&ack_packet), Some(SRTLA_TYPE_ACK));

    let parsed_acks = parse_srtla_ack(&ack_packet);
    assert_eq!(parsed_acks, original_acks);
}

#[tokio::test]
async fn test_sequence_number_parsing() {
    // Valid sequence number (control bit clear)
    let data_packet = [0x00, 0x00, 0x12, 0x34];
    assert_eq!(get_srt_sequence_number(&data_packet), Some(0x1234));

    // Invalid sequence number (control bit set)
    let control_packet = [0x80, 0x00, 0x12, 0x34];
    assert_eq!(get_srt_sequence_number(&control_packet), None);

    // Maximum valid sequence number
    let max_seq_packet = [0x7f, 0xff, 0xff, 0xff];
    assert_eq!(get_srt_sequence_number(&max_seq_packet), Some(0x7fffffff));
}

#[test]
fn test_packet_type_constants() {
    // Ensure SRTLA types are in the correct range
    assert!((SRTLA_TYPE_KEEPALIVE & 0xf000) == 0x9000);
    assert!((SRTLA_TYPE_ACK & 0xf000) == 0x9000);
    assert!((SRTLA_TYPE_REG1 & 0xf000) == 0x9000);
    assert!((SRTLA_TYPE_REG2 & 0xf000) == 0x9000);
    assert!((SRTLA_TYPE_REG3 & 0xf000) == 0x9000);

    // Ensure SRT types are in the correct range
    assert!((SRT_TYPE_HANDSHAKE & 0xf000) == 0x8000);
    assert!((SRT_TYPE_ACK & 0xf000) == 0x8000);
    assert!((SRT_TYPE_NAK & 0xf000) == 0x8000);
    assert!((SRT_TYPE_SHUTDOWN & 0xf000) == 0x8000);

    // Data packets should have no control bits
    assert_eq!(SRT_TYPE_DATA, 0x0000);
}

#[test]
fn test_protocol_constants_consistency() {
    // Test that length constants are consistent
    assert_eq!(SRTLA_TYPE_REG1_LEN, 2 + SRTLA_ID_LEN);
    assert_eq!(SRTLA_TYPE_REG2_LEN, 2 + SRTLA_ID_LEN);
    assert_eq!(SRTLA_TYPE_REG3_LEN, 2);

    // Test window constants make sense
    assert!(WINDOW_MIN > 0);
    assert!(WINDOW_DEF > WINDOW_MIN);
    assert!(WINDOW_MAX > WINDOW_DEF);
    assert!(WINDOW_MULT > 0);
    assert!(WINDOW_INCR > 0);
    assert!(WINDOW_DECR > 0);

    // Test timeout constants are reasonable
    assert!(CONN_TIMEOUT > 0);
    assert!(REG2_TIMEOUT > 0);
    assert!(REG3_TIMEOUT > 0);
    assert!(GLOBAL_TIMEOUT >= CONN_TIMEOUT);
    assert!(IDLE_TIME > 0);
}

#[test]
fn test_large_nak_range_limit() {
    // Test that NAK parsing limits range size to prevent memory exhaustion
    let mut large_range_packet = vec![0u8; 12];
    large_range_packet[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());

    // Create a range that would be > 1000 items
    let range_start = 1u32 | 0x8000_0000;
    large_range_packet[4..8].copy_from_slice(&range_start.to_be_bytes());
    large_range_packet[8..12].copy_from_slice(&2000u32.to_be_bytes());

    let naks = parse_srt_nak(&large_range_packet);
    assert!(
        naks.len() <= 1000,
        "NAK range should be limited to 1000 items"
    );
}

#[test]
fn test_malformed_packet_handling() {
    // Test handling of packets that are too short
    assert_eq!(get_packet_type(&[]), None);
    assert_eq!(get_packet_type(&[0x90]), None);

    assert_eq!(get_srt_sequence_number(&[]), None);
    assert_eq!(get_srt_sequence_number(&[0x00, 0x00]), None);

    // Test handling of malformed NAK packets
    let short_nak = [0x80, 0x03, 0x00, 0x00]; // Too short for any NAK data
    assert_eq!(parse_srt_nak(&short_nak), vec![]);

    let wrong_type = [0x80, 0x02, 0x00, 0x00, 0x00, 0x00, 0x12, 0x34]; // ACK, not NAK
    assert_eq!(parse_srt_nak(&wrong_type), vec![]);
}

#[test]
fn test_keepalive_timestamp_edge_cases() {
    // Test keepalive with minimum valid packet
    let mut min_keepalive = vec![0u8; 10];
    min_keepalive[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
    // Timestamp is all zeros

    assert!(is_srtla_keepalive(&min_keepalive));
    assert_eq!(extract_keepalive_timestamp(&min_keepalive), Some(0));

    // Test keepalive with maximum timestamp
    let mut max_keepalive = vec![0u8; 10];
    max_keepalive[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
    for i in 2..10 {
        max_keepalive[i] = 0xff;
    }

    assert_eq!(extract_keepalive_timestamp(&max_keepalive), Some(u64::MAX));

    // Test with wrong packet type
    let mut wrong_type = min_keepalive.clone();
    wrong_type[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());
    assert_eq!(extract_keepalive_timestamp(&wrong_type), None);
}

#[tokio::test]
async fn test_empty_ack_packet() {
    let empty_acks: Vec<u32> = vec![];
    let packet = create_ack_packet(&empty_acks);

    assert_eq!(packet.len(), 4); // Packet type + padding
    assert_eq!(get_packet_type(&packet), Some(SRTLA_TYPE_ACK));

    let parsed = parse_srtla_ack(&packet);
    assert!(parsed.is_empty());
}

#[test]
fn test_packet_validators_comprehensive() {
    // Create valid packets of each type
    let reg1_id = [0x11; SRTLA_ID_LEN];
    let reg1_packet = create_reg1_packet(&reg1_id);

    let reg2_id = [0x22; SRTLA_ID_LEN];
    let reg2_packet = create_reg2_packet(&reg2_id);

    let reg3_packet = vec![(SRTLA_TYPE_REG3 >> 8) as u8, (SRTLA_TYPE_REG3 & 0xff) as u8];
    let keepalive_packet = create_keepalive_packet();

    let mut ack_packet = vec![0u8; 20];
    ack_packet[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());

    // Test cross-validation - each validator should only accept its own type
    assert!(is_srtla_reg1(&reg1_packet));
    assert!(!is_srtla_reg1(&reg2_packet));
    assert!(!is_srtla_reg1(&reg3_packet));
    assert!(!is_srtla_reg1(&keepalive_packet));
    assert!(!is_srtla_reg1(&ack_packet));

    assert!(!is_srtla_reg2(&reg1_packet));
    assert!(is_srtla_reg2(&reg2_packet));
    assert!(!is_srtla_reg2(&reg3_packet));
    assert!(!is_srtla_reg2(&keepalive_packet));
    assert!(!is_srtla_reg2(&ack_packet));

    assert!(!is_srtla_reg3(&reg1_packet));
    assert!(!is_srtla_reg3(&reg2_packet));
    assert!(is_srtla_reg3(&reg3_packet));
    assert!(!is_srtla_reg3(&keepalive_packet));
    assert!(!is_srtla_reg3(&ack_packet));

    assert!(!is_srtla_keepalive(&reg1_packet));
    assert!(!is_srtla_keepalive(&reg2_packet));
    assert!(!is_srtla_keepalive(&reg3_packet));
    assert!(is_srtla_keepalive(&keepalive_packet));
    assert!(!is_srtla_keepalive(&ack_packet));

    assert!(!is_srt_ack(&reg1_packet));
    assert!(!is_srt_ack(&reg2_packet));
    assert!(!is_srt_ack(&reg3_packet));
    assert!(!is_srt_ack(&keepalive_packet));
    assert!(is_srt_ack(&ack_packet));
}

#[test]
fn test_id_length_constant() {
    // Ensure SRTLA_ID_LEN is reasonable (not too small, not too large)
    assert!(
        SRTLA_ID_LEN >= 32,
        "ID should be at least 32 bytes for security"
    );
    assert!(SRTLA_ID_LEN <= 1024, "ID should not be excessively large");
    assert_eq!(
        SRTLA_ID_LEN, 256,
        "ID length should match protocol specification"
    );
}
