use smallvec::SmallVec;

use super::constants::*;
use super::types::ConnectionInfo;

pub fn create_reg1_packet(id: &[u8; SRTLA_ID_LEN]) -> [u8; SRTLA_TYPE_REG1_LEN] {
    let mut pkt = [0u8; SRTLA_TYPE_REG1_LEN];
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_REG1.to_be_bytes());
    pkt[2..].copy_from_slice(id);
    pkt
}

pub fn create_reg2_packet(id: &[u8; SRTLA_ID_LEN]) -> [u8; SRTLA_TYPE_REG2_LEN] {
    let mut pkt = [0u8; SRTLA_TYPE_REG2_LEN];
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_REG2.to_be_bytes());
    pkt[2..].copy_from_slice(id);
    pkt
}

#[allow(dead_code)]
pub fn create_keepalive_packet() -> [u8; 10] {
    let mut pkt = [0u8; 10];
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
    let ts = chrono::Utc::now().timestamp_millis() as u64;
    for i in 0..8 {
        pkt[2 + i] = ((ts >> (56 - i * 8)) & 0xff) as u8;
    }
    pkt
}

/// Create extended KEEPALIVE packet with connection info
///
/// Extended format (38 bytes):
/// - Bytes 0-1:   Type (0x9000)
/// - Bytes 2-9:   Timestamp (u64 ms)
/// - Bytes 10-11: Magic (0xC01F)
/// - Bytes 12-13: Version (0x0001)
/// - Bytes 14-17: Connection ID (u32)
/// - Bytes 18-21: Window (i32)
/// - Bytes 22-25: In-flight (i32)
/// - Bytes 26-29: RTT ms (u32)
/// - Bytes 30-33: NAK count (u32)
/// - Bytes 34-37: Bitrate bytes/sec (u32)
///
/// This packet is backwards compatible:
/// - Old receivers read bytes 0-9 (type + timestamp) and ignore the rest
/// - New receivers detect magic at bytes 10-11 and parse connection info
pub fn create_keepalive_packet_ext(info: ConnectionInfo) -> [u8; SRTLA_KEEPALIVE_EXT_LEN] {
    let mut pkt = [0u8; SRTLA_KEEPALIVE_EXT_LEN];

    // Standard keepalive header (bytes 0-9)
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
    let ts = chrono::Utc::now().timestamp_millis() as u64;
    pkt[2..10].copy_from_slice(&ts.to_be_bytes());

    // Extended data (bytes 10-37)
    pkt[10..12].copy_from_slice(&SRTLA_KEEPALIVE_MAGIC.to_be_bytes());
    pkt[12..14].copy_from_slice(&SRTLA_KEEPALIVE_EXT_VERSION.to_be_bytes());
    pkt[14..18].copy_from_slice(&info.conn_id.to_be_bytes());
    pkt[18..22].copy_from_slice(&info.window.to_be_bytes());
    pkt[22..26].copy_from_slice(&info.in_flight.to_be_bytes());
    pkt[26..30].copy_from_slice(&info.rtt_ms.to_be_bytes());
    pkt[30..34].copy_from_slice(&info.nak_count.to_be_bytes());
    pkt[34..38].copy_from_slice(&info.bitrate_bytes_per_sec.to_be_bytes());

    pkt
}

/// Create SRTLA ACK packet (used in tests and receiver implementations)
#[allow(dead_code)]
pub fn create_ack_packet(acks: &[u32]) -> SmallVec<u8, 64> {
    // Create packets that match the actual SRTLA receiver format (4-byte header)
    let mut pkt = SmallVec::from_vec(vec![0u8; 4 + 4 * acks.len()]);
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_ACK.to_be_bytes());
    pkt[2] = 0x00; // Padding (matching receiver behavior)
    pkt[3] = 0x00; // Padding (matching receiver behavior)
    for (i, &ack) in acks.iter().enumerate() {
        let off = 4 + i * 4;
        pkt[off..off + 4].copy_from_slice(&ack.to_be_bytes());
    }
    pkt
}
