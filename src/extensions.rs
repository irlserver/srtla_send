//! IRLSERVER SRTLA PROTOCOL EXTENSIONS
//!
//! This module contains irlserver-specific extensions to the SRTLA protocol.
//! These extensions are NOT part of the standard SRTLA specification and may
//! not be compatible with other SRTLA implementations.
//!
//! # Extension Negotiation
//!
//! Extensions are negotiated using a capability handshake after successful
//! SRTLA registration (REG3):
//!
//! 1. Sender → EXT_HELLO (announces supported extensions via bitmask)
//! 2. Receiver → EXT_ACK (responds with its supported extensions)
//! 3. Only mutually supported extensions are used
//!
//! # Extension Range Allocation (0x9F00-0x9FFF)
//!
//! - `0x9F00-0x9F0F`: Connection telemetry and statistics
//! - `0x9F10-0x9F1F`: Quality and performance metrics
//! - `0x9F20-0x9FEF`: Reserved for future extensions
//! - `0x9FF0-0x9FFF`: Extension negotiation and handshake

use crate::protocol::get_packet_type;

// ============================================================================
// Extension Negotiation Packets
// ============================================================================

/// Extension capability negotiation - HELLO packet
///
/// Sent by sender after successful REG3 to announce supported extensions.
/// Receiver responds with EXT_ACK containing its supported extensions.
///
/// Packet format (10 bytes):
/// - Bytes 0-1:  Packet type (0x9FF0)
/// - Bytes 2-3:  Protocol version (u16, currently 0x0001)
/// - Bytes 4-7:  Extension capability flags (u32, big-endian bitmask)
/// - Bytes 8-9:  Reserved (u16, must be 0x0000)
pub const SRTLA_EXT_HELLO: u16 = 0x9ff0;
pub const SRTLA_EXT_HELLO_LEN: usize = 10;

/// Extension capability negotiation - ACK packet
///
/// Sent by receiver in response to EXT_HELLO to acknowledge and indicate
/// which extensions it supports. Format is identical to EXT_HELLO.
pub const SRTLA_EXT_ACK: u16 = 0x9ff1;
#[allow(dead_code)]
pub const SRTLA_EXT_ACK_LEN: usize = 10;

/// Current extension protocol version
pub const SRTLA_EXT_VERSION: u16 = 0x0001;

// ============================================================================
// Extension Capability Flags (bitmask values)
// ============================================================================

/// Connection info telemetry (packet type 0x9F00)
pub const SRTLA_EXT_CAP_CONN_INFO: u32 = 0x00000001;

// Future extensions can be added here:
// pub const SRTLA_EXT_CAP_QUALITY_METRICS: u32 = 0x00000002;
// pub const SRTLA_EXT_CAP_BANDWIDTH_HINTS: u32 = 0x00000004;

// ============================================================================
// Connection Info Telemetry Extension (0x9F00)
// ============================================================================

/// Connection information telemetry packet
///
/// Sends per-connection statistics from sender to receiver for monitoring,
/// debugging, and potential future adaptive bonding features.
///
/// Packet format (32 bytes):
/// - Bytes 0-1:   Packet type (0x9F00)
/// - Bytes 2-3:   Version (u16, currently 0x0001)
/// - Bytes 4-7:   Connection ID (u32, big-endian)
/// - Bytes 8-11:  Window size (i32, big-endian)
/// - Bytes 12-15: In-flight packets (i32, big-endian)
/// - Bytes 16-23: Smooth RTT in microseconds (u64, big-endian)
/// - Bytes 24-27: NAK count (u32, big-endian)
/// - Bytes 28-31: Bitrate in bytes/sec (u32, big-endian)
pub const SRTLA_EXT_CONN_INFO: u16 = 0x9f00;
pub const SRTLA_EXT_CONN_INFO_LEN: usize = 32;
pub const SRTLA_EXT_CONN_INFO_VERSION: u16 = 0x0001;

// ============================================================================
// Data Structures
// ============================================================================

/// Parsed extension negotiation data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionCapabilities {
    pub version: u16,
    pub capabilities: u32,
}

/// Parsed connection info data
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ConnectionInfoData {
    pub version: u16,
    pub conn_id: u32,
    pub window: i32,
    pub in_flight: i32,
    pub rtt_us: u64,
    pub nak_count: u32,
    pub bitrate_bytes_per_sec: u32,
}

// ============================================================================
// Extension Negotiation Functions
// ============================================================================

/// Create extension HELLO packet
///
/// Announces supported extensions to the receiver via capability bitmask.
///
/// # Arguments
///
/// * `capabilities` - Bitmask of supported extensions (use SRTLA_EXT_CAP_* constants)
///
/// # Example
///
/// ```
/// use srtla_send::extensions::{SRTLA_EXT_CAP_CONN_INFO, create_extension_hello};
///
/// let packet = create_extension_hello(SRTLA_EXT_CAP_CONN_INFO);
/// ```
pub fn create_extension_hello(capabilities: u32) -> [u8; SRTLA_EXT_HELLO_LEN] {
    let mut pkt = [0u8; SRTLA_EXT_HELLO_LEN];

    // Packet type (bytes 0-1)
    pkt[0..2].copy_from_slice(&SRTLA_EXT_HELLO.to_be_bytes());

    // Protocol version (bytes 2-3)
    pkt[2..4].copy_from_slice(&SRTLA_EXT_VERSION.to_be_bytes());

    // Extension capabilities (bytes 4-7)
    pkt[4..8].copy_from_slice(&capabilities.to_be_bytes());

    // Reserved (bytes 8-9)
    pkt[8..10].copy_from_slice(&0u16.to_be_bytes());

    pkt
}

/// Parse extension HELLO or ACK packet
///
/// Extracts extension capabilities from an EXT_HELLO or EXT_ACK packet.
///
/// # Returns
///
/// * `Some(ExtensionCapabilities)` if the packet is valid
/// * `None` if the packet is malformed or has the wrong type
pub fn parse_extension_packet(buf: &[u8]) -> Option<ExtensionCapabilities> {
    if buf.len() < SRTLA_EXT_HELLO_LEN {
        return None;
    }

    let pkt_type = get_packet_type(buf)?;
    if pkt_type != SRTLA_EXT_HELLO && pkt_type != SRTLA_EXT_ACK {
        return None;
    }

    let version = u16::from_be_bytes([buf[2], buf[3]]);
    let capabilities = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

    Some(ExtensionCapabilities {
        version,
        capabilities,
    })
}

/// Check if a specific extension is supported in the capability bitmask
#[inline]
#[allow(dead_code)]
pub fn has_extension(capabilities: u32, flag: u32) -> bool {
    (capabilities & flag) != 0
}

// ============================================================================
// Connection Info Functions
// ============================================================================

/// Create connection info telemetry packet
///
/// Constructs a packet containing per-connection statistics for monitoring
/// and debugging purposes.
///
/// # Arguments
///
/// * `conn_id` - Unique connection identifier (u32 per protocol specification)
/// * `window` - Current congestion window size
/// * `in_flight` - Number of packets currently in flight
/// * `rtt_us` - Smoothed round-trip time in microseconds
/// * `nak_count` - Total NAK count for this connection
/// * `bitrate_bytes_per_sec` - Current bitrate in bytes per second
pub fn create_connection_info_packet(
    conn_id: u32,
    window: i32,
    in_flight: i32,
    rtt_us: u64,
    nak_count: u32,
    bitrate_bytes_per_sec: u32,
) -> [u8; SRTLA_EXT_CONN_INFO_LEN] {
    let mut pkt = [0u8; SRTLA_EXT_CONN_INFO_LEN];

    // Packet type (bytes 0-1)
    pkt[0..2].copy_from_slice(&SRTLA_EXT_CONN_INFO.to_be_bytes());

    // Version (bytes 2-3)
    pkt[2..4].copy_from_slice(&SRTLA_EXT_CONN_INFO_VERSION.to_be_bytes());

    // Connection ID (bytes 4-7)
    pkt[4..8].copy_from_slice(&conn_id.to_be_bytes());

    // Window size (bytes 8-11)
    pkt[8..12].copy_from_slice(&window.to_be_bytes());

    // In-flight packets (bytes 12-15)
    pkt[12..16].copy_from_slice(&in_flight.to_be_bytes());

    // Smooth RTT (bytes 16-23)
    pkt[16..24].copy_from_slice(&rtt_us.to_be_bytes());

    // NAK count (bytes 24-27)
    pkt[24..28].copy_from_slice(&nak_count.to_be_bytes());

    // Bitrate in bytes/sec (bytes 28-31)
    pkt[28..32].copy_from_slice(&bitrate_bytes_per_sec.to_be_bytes());

    pkt
}

/// Parse connection info packet
///
/// Extracts connection statistics from a SRTLA_EXT_CONN_INFO packet.
#[allow(dead_code)]
pub fn parse_connection_info(buf: &[u8]) -> Option<ConnectionInfoData> {
    if buf.len() < SRTLA_EXT_CONN_INFO_LEN {
        return None;
    }
    if get_packet_type(buf)? != SRTLA_EXT_CONN_INFO {
        return None;
    }

    let version = u16::from_be_bytes([buf[2], buf[3]]);
    let conn_id = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let window = i32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let in_flight = i32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let rtt_us = u64::from_be_bytes([
        buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
    ]);
    let nak_count = u32::from_be_bytes([buf[24], buf[25], buf[26], buf[27]]);
    let bitrate_bytes_per_sec = u32::from_be_bytes([buf[28], buf[29], buf[30], buf[31]]);

    Some(ConnectionInfoData {
        version,
        conn_id,
        window,
        in_flight,
        rtt_us,
        nak_count,
        bitrate_bytes_per_sec,
    })
}

/// Check if packet is a connection info packet
#[inline]
#[allow(dead_code)]
pub fn is_connection_info_packet(buf: &[u8]) -> bool {
    buf.len() == SRTLA_EXT_CONN_INFO_LEN && get_packet_type(buf) == Some(SRTLA_EXT_CONN_INFO)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_hello_creation() {
        let pkt = create_extension_hello(SRTLA_EXT_CAP_CONN_INFO);
        assert_eq!(pkt.len(), SRTLA_EXT_HELLO_LEN);
        assert_eq!(get_packet_type(&pkt), Some(SRTLA_EXT_HELLO));

        // Verify version
        assert_eq!(u16::from_be_bytes([pkt[2], pkt[3]]), SRTLA_EXT_VERSION);

        // Verify capabilities
        assert_eq!(
            u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
            SRTLA_EXT_CAP_CONN_INFO
        );
    }

    #[test]
    fn test_extension_packet_parsing() {
        let caps = SRTLA_EXT_CAP_CONN_INFO | 0x00000004; // Multiple capabilities
        let pkt = create_extension_hello(caps);

        let parsed = parse_extension_packet(&pkt).unwrap();
        assert_eq!(parsed.version, SRTLA_EXT_VERSION);
        assert_eq!(parsed.capabilities, caps);
    }

    #[test]
    fn test_has_extension() {
        let caps = SRTLA_EXT_CAP_CONN_INFO | 0x00000004;

        assert!(has_extension(caps, SRTLA_EXT_CAP_CONN_INFO));
        assert!(has_extension(caps, 0x00000004));
        assert!(!has_extension(caps, 0x00000002));
    }

    #[test]
    fn test_connection_info_roundtrip() {
        let pkt = create_connection_info_packet(42, 15000, 8, 200_000, 25, 8_000_000);

        let info = parse_connection_info(&pkt).unwrap();
        assert_eq!(info.version, SRTLA_EXT_CONN_INFO_VERSION);
        assert_eq!(info.conn_id, 42);
        assert_eq!(info.window, 15000);
        assert_eq!(info.in_flight, 8);
        assert_eq!(info.rtt_us, 200_000);
        assert_eq!(info.nak_count, 25);
        assert_eq!(info.bitrate_bytes_per_sec, 8_000_000);
    }

    #[test]
    fn test_is_connection_info_packet() {
        let pkt = create_connection_info_packet(1, 20000, 5, 150_000, 10, 5_000_000);
        assert!(is_connection_info_packet(&pkt));

        // Wrong length
        let short_pkt = vec![0u8; 10];
        assert!(!is_connection_info_packet(&short_pkt));
    }

    #[test]
    fn test_extension_range() {
        // Verify extension packet types are in the reserved irlserver range
        assert!(SRTLA_EXT_HELLO >= 0x9f00 && SRTLA_EXT_HELLO <= 0x9fff);
        assert!(SRTLA_EXT_ACK >= 0x9f00 && SRTLA_EXT_ACK <= 0x9fff);
        assert!(SRTLA_EXT_CONN_INFO >= 0x9f00 && SRTLA_EXT_CONN_INFO <= 0x9fff);
    }
}
