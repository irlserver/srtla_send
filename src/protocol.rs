#![allow(dead_code)]

use smallvec::SmallVec;

pub const SRTLA_TYPE_KEEPALIVE: u16 = 0x9000;
pub const SRTLA_TYPE_ACK: u16 = 0x9100;
pub const SRTLA_TYPE_REG1: u16 = 0x9200;
pub const SRTLA_TYPE_REG2: u16 = 0x9201;
pub const SRTLA_TYPE_REG3: u16 = 0x9202;
pub const SRTLA_TYPE_REG_ERR: u16 = 0x9210;
pub const SRTLA_TYPE_REG_NGP: u16 = 0x9211;
pub const SRTLA_TYPE_REG_NAK: u16 = 0x9212;

pub const SRT_TYPE_HANDSHAKE: u16 = 0x8000;
pub const SRT_TYPE_ACK: u16 = 0x8002;
pub const SRT_TYPE_NAK: u16 = 0x8003;
pub const SRT_TYPE_SHUTDOWN: u16 = 0x8005;
pub const SRT_TYPE_DATA: u16 = 0x0000;

pub const SRT_MIN_LEN: usize = 16;

pub const SRTLA_ID_LEN: usize = 256;
pub const SRTLA_TYPE_REG1_LEN: usize = 2 + SRTLA_ID_LEN;
pub const SRTLA_TYPE_REG2_LEN: usize = 2 + SRTLA_ID_LEN;
pub const SRTLA_TYPE_REG3_LEN: usize = 2;

pub const MTU: usize = 1500;

pub const CONN_TIMEOUT: u64 = 5; // sec
pub const REG2_TIMEOUT: u64 = 4; // sec
pub const REG3_TIMEOUT: u64 = 4; // sec
pub const GLOBAL_TIMEOUT: u64 = 10; // sec
pub const IDLE_TIME: u64 = 1; // sec

pub const WINDOW_MIN: i32 = 1;
pub const WINDOW_DEF: i32 = 20;
pub const WINDOW_MAX: i32 = 60;
pub const WINDOW_MULT: i32 = 1000;
pub const WINDOW_DECR: i32 = 100;
pub const WINDOW_INCR: i32 = 30;

pub const PKT_LOG_SIZE: usize = 256;

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

pub fn create_keepalive_packet() -> [u8; 10] {
    let mut pkt = [0u8; 10];
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
    let ts = chrono::Utc::now().timestamp_millis() as u64;
    for i in 0..8 {
        pkt[2 + i] = ((ts >> (56 - i * 8)) & 0xff) as u8;
    }
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

pub fn is_srtla_reg1(buf: &[u8]) -> bool {
    buf.len() == SRTLA_TYPE_REG1_LEN && get_packet_type(buf) == Some(SRTLA_TYPE_REG1)
}
pub fn is_srtla_reg2(buf: &[u8]) -> bool {
    buf.len() == SRTLA_TYPE_REG2_LEN && get_packet_type(buf) == Some(SRTLA_TYPE_REG2)
}
pub fn is_srtla_reg3(buf: &[u8]) -> bool {
    buf.len() == SRTLA_TYPE_REG3_LEN && get_packet_type(buf) == Some(SRTLA_TYPE_REG3)
}
pub fn is_srtla_keepalive(buf: &[u8]) -> bool {
    get_packet_type(buf) == Some(SRTLA_TYPE_KEEPALIVE)
}
pub fn is_srt_ack(buf: &[u8]) -> bool {
    get_packet_type(buf) == Some(SRT_TYPE_ACK)
}
