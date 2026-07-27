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

/// Set the R bit on a copy of an SRT data packet, marking it a retransmission.
///
/// Used for the duplicate probes sent on links held out of the payload
/// rotation. The receiver dedups them by sequence number either way, but the
/// flag changes how it accounts for them, and on an SRTLA-patched receiver that
/// matters: every *non*-retransmitted data packet feeds the reorder-hold
/// estimator, which measures the transit spread between bonded links and delays
/// loss reports by up to that spread. A probe from a link running a second
/// behind therefore pins the receiver's NAK hold near its ceiling — slowing
/// recovery of genuine losses on the *healthy* links — even though the probed
/// link carries no unique payload, so no gap will ever be filled by waiting for
/// it. Flagged as a retransmit, the probe is excluded from that estimator, and
/// from the reorder-tolerance ratchet on a stock receiver.
///
/// Safe to flip on an encrypted packet we never decrypted: the receiver zeroes
/// this bit before computing the AES-GCM auth tag (and restores it after), so
/// the tag never covered it. Dedup, the SRTLA per-packet ACK, and TSBPD are all
/// indifferent to it.
///
/// Assumes the peers negotiated `SRT_OPT_REXMITFLG`, without which bit 26 is
/// part of a 27-bit message number rather than a flag. Every libsrt advertises
/// it unconditionally, and this crate already *reads* the bit as R in
/// [`is_srt_data_retransmit`], so the assumption is not a new one.
///
/// No-op on anything that is not an SRT data packet.
#[inline]
pub fn set_srt_data_retransmit(buf: &mut [u8]) {
    if buf.len() >= 8 && (buf[0] & 0x80) == 0 {
        buf[4] |= 0x04;
    }
}
