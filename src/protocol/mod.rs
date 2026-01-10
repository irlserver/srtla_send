mod builders;
mod constants;
mod parsers;
mod types;

// Re-export all public items for backwards compatibility
// Consumers can still use `use crate::protocol::*`

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
    extract_keepalive_conn_info, extract_keepalive_timestamp, parse_srt_ack, parse_srt_nak,
    parse_srtla_ack,
};
// Types and helpers
#[allow(unused_imports)]
pub use types::{
    ConnectionInfo, get_packet_type, get_srt_sequence_number, is_srt_ack, is_srtla_keepalive,
    is_srtla_reg1, is_srtla_reg2, is_srtla_reg3,
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

        let pkt = create_keepalive_packet_ext(info);

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
        let pkt = create_keepalive_packet();

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

        let ext_pkt = create_keepalive_packet_ext(info);

        // Old receiver behavior: only reads first 10 bytes
        let timestamp_from_ext = extract_keepalive_timestamp(&ext_pkt);
        assert!(timestamp_from_ext.is_some());

        // Compare with standard keepalive timestamp
        let std_pkt = create_keepalive_packet();
        let timestamp_from_std = extract_keepalive_timestamp(&std_pkt);
        assert!(timestamp_from_std.is_some());

        // Both should be valid timestamps (within 1 second of each other)
        let diff = timestamp_from_ext
            .unwrap()
            .abs_diff(timestamp_from_std.unwrap());
        assert!(diff < 1000); // Less than 1 second difference
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
}
