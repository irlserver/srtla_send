use smallvec::SmallVec;

use super::constants::*;
use super::types::{ConnectionInfo, SrtHandshakeLatency, get_packet_type};

/// Read a big-endian 32-bit word at `off`, or `None` if it does not fit.
#[inline]
fn be32(buf: &[u8], off: usize) -> Option<u32> {
    let bytes = buf.get(off..off + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Extract the TSBPD latency an SRT conclusion handshake declares.
///
/// SRT negotiates its receiver buffer depth in the clear during the handshake,
/// and every one of those packets crosses this proxy: an `SRT_CMD_HSRSP` block
/// from the far end tells us, in milliseconds, exactly how long it will hold a
/// packet before delivering it. That is the deadline any link we route over has
/// to beat, and it is the only authoritative figure we can get — the scheduler
/// otherwise has to guess a budget from its own RTT measurements.
///
/// Layout, all words big-endian:
///
/// ```text
///  0            16               64
///  +------------+----------------+------------------+
///  | ctrl hdr   | handshake body | extension blocks |
///  +------------+----------------+------------------+
/// ```
///
/// Each extension block is one spec word — command in the high 16 bits, body
/// length in 32-bit words in the low 16 — followed by that body. HSREQ/HSRSP
/// bodies are `version, flags, latency`, and the latency word packs the
/// receive delay in its high half and the send delay in its low half.
///
/// Returns `None` for anything that is not an HSv5 conclusion handshake
/// carrying such a block, which includes induction packets, rejections, HSv4
/// (whose SRT handshake is a separate `UMSG_EXT` control packet, not an
/// extension block), and any truncated or self-inconsistent input.
pub fn parse_srt_handshake_latency(buf: &[u8]) -> Option<SrtHandshakeLatency> {
    if get_packet_type(buf)? != SRT_TYPE_HANDSHAKE {
        return None;
    }
    let body = buf.get(SRT_CONTROL_HEADER_LEN..)?;
    if body.len() < SRT_HANDSHAKE_CIF_LEN {
        return None;
    }

    if be32(body, 0)? != SRT_HS_VERSION_5 {
        return None;
    }
    // Word 5 is the request type. Only the conclusion phase carries extensions;
    // checking it also stops us reading an induction packet's extension field,
    // which holds a magic cookie rather than flags.
    if be32(body, 20)? as i32 != SRT_HS_REQTYPE_CONCLUSION {
        return None;
    }
    // Word 1 is `encryption field | extension field`; the HSREQ bit lives in
    // the low half.
    if (be32(body, 4)? & 0xffff) & SRT_HS_EXT_FLAG_HSREQ == 0 {
        return None;
    }

    // Walk the blocks. HSREQ/HSRSP is not required to come first — a stream ID,
    // key material, congestion, filter or group block may precede it. Each step
    // consumes at least the 4-byte spec word, so `rest` strictly shrinks and
    // the loop terminates on any input.
    let mut rest = body.get(SRT_HANDSHAKE_CIF_LEN..)?;
    while rest.len() >= 4 {
        let spec = be32(rest, 0)?;
        let cmd = (spec >> 16) as u16;
        let block_len = ((spec & 0xffff) as usize) * 4;
        // A length running past the packet means the handshake is truncated or
        // lying; either way there is nothing further to read.
        let block = rest.get(4..4 + block_len)?;

        if cmd == SRT_HS_EXT_CMD_HSREQ || cmd == SRT_HS_EXT_CMD_HSRSP {
            if block.len() < SRT_HS_EXT_HSREQ_WORDS * 4 {
                return None;
            }
            let flags = be32(block, 4)?;
            let latency = be32(block, 8)?;
            return Some(SrtHandshakeLatency {
                is_response: cmd == SRT_HS_EXT_CMD_HSRSP,
                rcv_ms: ((flags & SRT_HS_OPT_TSBPDRCV) != 0).then_some((latency >> 16) as u16),
                snd_ms: ((flags & SRT_HS_OPT_TSBPDSND) != 0).then_some((latency & 0xffff) as u16),
            });
        }

        rest = &rest[4 + block_len..];
    }
    None
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
