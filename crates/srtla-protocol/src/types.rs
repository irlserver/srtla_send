use super::constants::*;

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

/// Whether `buf` is an SRT data packet flagged as a retransmission.
///
/// The second header word of an SRT data packet is
/// `PP(2) | O(1) | KK(2) | R(1) | message number(26)`; the R bit (bit 26,
/// i.e. `0x04` in byte 4) marks a packet the SRT sender is re-sending in
/// response to a receiver NAK. Retransmits are latency-critical recovery
/// traffic: they fill an existing hole in the receiver buffer, so one that
/// rides a slow path arrives too late to matter.
#[inline]
pub fn is_srt_data_retransmit(buf: &[u8]) -> bool {
    buf.len() >= 8 && (buf[0] & 0x80) == 0 && (buf[4] & 0x04) != 0
}
