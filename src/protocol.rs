use smallvec::SmallVec;

pub const SRTLA_TYPE_KEEPALIVE: u16 = 0x9000;
pub const SRTLA_TYPE_ACK: u16 = 0x9100;
pub const SRTLA_TYPE_REG1: u16 = 0x9200;
pub const SRTLA_TYPE_REG2: u16 = 0x9201;
pub const SRTLA_TYPE_REG3: u16 = 0x9202;
pub const SRTLA_TYPE_REG_ERR: u16 = 0x9210;
pub const SRTLA_TYPE_REG_NGP: u16 = 0x9211;
#[allow(dead_code)]
pub const SRTLA_TYPE_REG_NAK: u16 = 0x9212;

// SRT protocol constants (some used in tests or for protocol completeness)
#[allow(dead_code)]
pub const SRT_TYPE_HANDSHAKE: u16 = 0x8000;
pub const SRT_TYPE_ACK: u16 = 0x8002;
pub const SRT_TYPE_NAK: u16 = 0x8003;
#[allow(dead_code)]
pub const SRT_TYPE_SHUTDOWN: u16 = 0x8005;
#[allow(dead_code)]
pub const SRT_TYPE_DATA: u16 = 0x0000;

pub const SRTLA_ID_LEN: usize = 256;
pub const SRTLA_TYPE_REG1_LEN: usize = 2 + SRTLA_ID_LEN;
pub const SRTLA_TYPE_REG2_LEN: usize = 2 + SRTLA_ID_LEN;
#[allow(dead_code)]
pub const SRTLA_TYPE_REG3_LEN: usize = 2;

pub const MTU: usize = 1500;

pub const CONN_TIMEOUT: u64 = 5; // sec
pub const REG2_TIMEOUT: u64 = 4; // sec
pub const REG3_TIMEOUT: u64 = 4; // sec
pub const IDLE_TIME: u64 = 1; // sec

pub const WINDOW_MIN: i32 = 1;
pub const WINDOW_DEF: i32 = 20;
pub const WINDOW_MAX: i32 = 60;
pub const WINDOW_MULT: i32 = 1000;
pub const WINDOW_DECR: i32 = 100;
pub const WINDOW_INCR: i32 = 30;

pub const PKT_LOG_SIZE: usize = 256;

// Extended KEEPALIVE with connection info
pub const SRTLA_KEEPALIVE_MAGIC: u16 = 0xc01f; // "Connection Info" marker
pub const SRTLA_KEEPALIVE_EXT_LEN: usize = 38; // Extended keepalive packet length
pub const SRTLA_KEEPALIVE_EXT_VERSION: u16 = 0x0001; // Protocol version

#[inline]
pub fn get_packet_type(buf: &[u8]) -> Option<u16> {
    if buf.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([buf[0], buf[1]]))
}

#[inline]
pub fn get_srt_sequence_number(buf: &[u8]) -> Option<u32> {
    if buf.len() < 4 {
        return None;
    }
    let sn = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if (sn & 0x8000_0000) == 0 {
        Some(sn)
    } else {
        None
    }
}

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

/// Connection info data for extended keepalive
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub conn_id: u32,
    pub window: i32,
    pub in_flight: i32,
    pub rtt_ms: u32,
    pub nak_count: u32,
    pub bitrate_bytes_per_sec: u32,
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

pub fn extract_keepalive_timestamp(buf: &[u8]) -> Option<u64> {
    if buf.len() < 10 {
        return None;
    }
    if get_packet_type(buf)? != SRTLA_TYPE_KEEPALIVE {
        return None;
    }
    let mut ts: u64 = 0;
    for i in 0..8 {
        ts = (ts << 8) | (buf[2 + i] as u64);
    }
    Some(ts)
}

/// Extract connection info from extended keepalive packet
///
/// Returns None if:
/// - Packet is too short (< 38 bytes)
/// - Not a KEEPALIVE packet
/// - Magic number doesn't match (not an extended keepalive)
/// - Version doesn't match
#[allow(dead_code)]
pub fn extract_keepalive_conn_info(buf: &[u8]) -> Option<ConnectionInfo> {
    if buf.len() < SRTLA_KEEPALIVE_EXT_LEN {
        return None;
    }
    if get_packet_type(buf)? != SRTLA_TYPE_KEEPALIVE {
        return None;
    }

    // Check magic number at bytes 10-11
    let magic = u16::from_be_bytes([buf[10], buf[11]]);
    if magic != SRTLA_KEEPALIVE_MAGIC {
        return None;
    }

    // Check version at bytes 12-13
    let version = u16::from_be_bytes([buf[12], buf[13]]);
    if version != SRTLA_KEEPALIVE_EXT_VERSION {
        return None;
    }

    // Parse connection info
    let conn_id = u32::from_be_bytes([buf[14], buf[15], buf[16], buf[17]]);
    let window = i32::from_be_bytes([buf[18], buf[19], buf[20], buf[21]]);
    let in_flight = i32::from_be_bytes([buf[22], buf[23], buf[24], buf[25]]);
    let rtt_ms = u32::from_be_bytes([buf[26], buf[27], buf[28], buf[29]]);
    let nak_count = u32::from_be_bytes([buf[30], buf[31], buf[32], buf[33]]);
    let bitrate_bytes_per_sec = u32::from_be_bytes([buf[34], buf[35], buf[36], buf[37]]);

    Some(ConnectionInfo {
        conn_id,
        window,
        in_flight,
        rtt_ms,
        nak_count,
        bitrate_bytes_per_sec,
    })
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

#[inline]
pub fn parse_srt_ack(buf: &[u8]) -> Option<u32> {
    if buf.len() < 20 {
        return None;
    }
    if get_packet_type(buf)? != SRT_TYPE_ACK {
        return None;
    }
    Some(u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]))
}

#[inline]
pub fn parse_srt_nak(buf: &[u8]) -> SmallVec<u32, 4> {
    if buf.len() < 8 {
        return SmallVec::new();
    }
    if get_packet_type(buf) != Some(SRT_TYPE_NAK) {
        return SmallVec::new();
    }
    let mut out = SmallVec::new();
    let mut i = 4usize;
    while i + 3 < buf.len() {
        let mut id = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        i += 4;
        if (id & 0x8000_0000) != 0 {
            id &= 0x7fff_ffff;
            if i + 3 >= buf.len() {
                break;
            }
            let end = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
            i += 4;
            let mut seq = id;
            while seq <= end && out.len() < 1000 {
                out.push(seq);
                seq = seq.wrapping_add(1);
            }
        } else {
            out.push(id);
        }
    }
    out
}

#[inline]
pub fn parse_srtla_ack(buf: &[u8]) -> SmallVec<u32, 4> {
    if buf.len() < 8 {
        return SmallVec::new();
    }
    if get_packet_type(buf) != Some(SRTLA_TYPE_ACK) {
        return SmallVec::new();
    }
    let mut out = SmallVec::new();

    // Match original C implementation behavior: skip first 4 bytes, not 2
    // The C code does: uint32_t *acks = (uint32_t *)buf; for (int i = 1; ...)
    // which effectively skips acks[0] (first 4 bytes)
    let mut i = 4usize; // Skip packet type + padding (4 bytes total)
    while i + 3 < buf.len() {
        let ack = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        out.push(ack);
        i += 4;
    }
    out
}

/// Helper functions for packet type checking (used in tests)
#[allow(dead_code)]
pub fn is_srtla_reg1(buf: &[u8]) -> bool {
    buf.len() == SRTLA_TYPE_REG1_LEN && get_packet_type(buf) == Some(SRTLA_TYPE_REG1)
}

#[allow(dead_code)]
pub fn is_srtla_reg2(buf: &[u8]) -> bool {
    buf.len() == SRTLA_TYPE_REG2_LEN && get_packet_type(buf) == Some(SRTLA_TYPE_REG2)
}

#[allow(dead_code)]
pub fn is_srtla_reg3(buf: &[u8]) -> bool {
    buf.len() == SRTLA_TYPE_REG3_LEN && get_packet_type(buf) == Some(SRTLA_TYPE_REG3)
}

#[allow(dead_code)]
pub fn is_srtla_keepalive(buf: &[u8]) -> bool {
    get_packet_type(buf) == Some(SRTLA_TYPE_KEEPALIVE)
}

#[allow(dead_code)]
pub fn is_srt_ack(buf: &[u8]) -> bool {
    get_packet_type(buf) == Some(SRT_TYPE_ACK)
}

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
