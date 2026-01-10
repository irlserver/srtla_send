use smallvec::SmallVec;

use super::constants::*;
use super::types::{ConnectionInfo, get_packet_type};

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
