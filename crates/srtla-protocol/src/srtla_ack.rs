use smallvec::SmallVec;

use super::constants::*;
use super::srt::ControlPacket;

/// Wire codec for SRTLA per-link ACK packets.
pub struct SrtlaAck;

impl SrtlaAck {
    /// Create SRTLA ACK packet (used in tests and receiver implementations).
    pub fn encode(acks: &[u32]) -> SmallVec<u8, 64> {
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
    pub fn parse(buf: &[u8]) -> SmallVec<u32, 16> {
        if buf.len() < 8 {
            return SmallVec::new();
        }
        if ControlPacket::packet_type(buf) != Some(SRTLA_TYPE_ACK) {
            return SmallVec::new();
        }
        let mut out: SmallVec<u32, 16> = SmallVec::new();

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
}
