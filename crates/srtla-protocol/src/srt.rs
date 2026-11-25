use smallvec::SmallVec;

use super::constants::*;

/// Marks a loss-list word as the start of a range; also the bit that must be
/// clear on any word carrying a bare 31-bit SRT sequence number.
const SRT_NAK_RANGE_FLAG: u32 = 0x8000_0000;

/// Widest loss list this parser will expand a NAK into.
///
/// Every id handed back becomes a retransmission upstream, so an unbounded
/// expansion is an amplification vector: two words on the wire can ask for four
/// billion resends. The cap is a hard ceiling on that cost. See
/// [`ControlPacket::parse_srt_nak`] for the saturation semantics.
const SRT_NAK_MAX_LOSS_IDS: usize = 1000;

/// Read a big-endian 32-bit word at `off`, or `None` if it does not fit.
#[inline]
fn be32(buf: &[u8], off: usize) -> Option<u32> {
    let bytes = buf.get(off..off + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// TSBPD latency declared in an SRT handshake's HSREQ/HSRSP extension block.
///
/// Both halves are in milliseconds and each is `None` when the peer did not set
/// the matching TSBPD flag, in which case the 16 bits are meaningless rather
/// than zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrtHandshakeLatency {
    /// True for `SRT_CMD_HSRSP` (the responder's answer), false for
    /// `SRT_CMD_HSREQ` (the initiator's proposal). Only the response carries
    /// negotiated values: the responder resolves the two sides as
    /// `max(own, proposed)` before echoing them back.
    pub is_response: bool,
    /// The sender of this block will *receive* with this TSBPD delay — i.e. how
    /// long it holds a packet before delivering it downstream. From an HSRSP,
    /// this is the deadline every packet we route has to beat.
    pub rcv_ms: Option<u16>,
    /// The delay the sender of this block expects its own peer to receive with.
    pub snd_ms: Option<u16>,
}

/// Wire helpers for SRT data packets (MSB of byte 0 clear).
pub struct SrtData;

impl SrtData {
    #[inline]
    pub fn sequence_number(buf: &[u8]) -> Option<u32> {
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
    pub fn is_retransmit(buf: &[u8]) -> bool {
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
    /// [`SrtData::is_retransmit`], so the assumption is not a new one.
    ///
    /// No-op on anything that is not an SRT data packet.
    #[inline]
    pub fn set_retransmit(buf: &mut [u8]) {
        if buf.len() >= 8 && (buf[0] & 0x80) == 0 {
            buf[4] |= 0x04;
        }
    }
}

/// Wire parsers for SRT/SRTLA control packets (type word, SRT ACK/NAK, and SRT handshakes).
pub struct ControlPacket;

impl ControlPacket {
    #[inline]
    pub fn packet_type(buf: &[u8]) -> Option<u16> {
        if buf.len() < 2 {
            return None;
        }
        Some(u16::from_be_bytes([buf[0], buf[1]]))
    }

    pub fn is_srt_ack(buf: &[u8]) -> bool {
        Self::packet_type(buf) == Some(SRT_TYPE_ACK)
    }

    #[inline]
    pub fn parse_srt_ack(buf: &[u8]) -> Option<u32> {
        if buf.len() < 20 {
            return None;
        }
        if Self::packet_type(buf)? != SRT_TYPE_ACK {
            return None;
        }
        Some(u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]))
    }

    /// Parse the loss list of an SRT NAK into individual sequence numbers.
    ///
    /// The loss list is the control packet's CIF, so it starts after the full
    /// 16-byte SRT control header — not after the 4-byte type word. An SRTLA ACK
    /// (see [`crate::SrtlaAck::parse`]) does start at offset 4, and the two must
    /// not be confused: reading a NAK from offset 4 turns the header's timestamp
    /// and destination socket id into phantom loss reports.
    ///
    /// An entry with the MSB set opens an inclusive range whose end is the next
    /// word. Both words are validated as 31-bit sequence numbers: the start is
    /// masked, and the end is *required* to have its MSB already clear. A range
    /// that fails either check — an end word with the MSB set, or an end that sorts
    /// below its masked start — yields nothing and parsing resumes at the following
    /// entry, matching the reference implementation, which has no serial-wraparound
    /// handling here. Rejecting the end word matters because it is attacker- and
    /// corruption-reachable: an unchecked `0xffff_ffff` end would otherwise expand
    /// into a full cap's worth of fabricated loss reports, each one a real
    /// retransmission on the wire.
    ///
    /// Expansion is bounded at [`SRT_NAK_MAX_LOSS_IDS`] ids per packet. The cap
    /// saturates silently — this crate is deliberately dependency-free and has no
    /// logger — so a legitimately huge range is truncated rather than reported. A
    /// receiver that still needs those ids will NAK them again.
    #[inline]
    pub fn parse_srt_nak(buf: &[u8]) -> SmallVec<u32, 8> {
        if buf.len() < SRT_CONTROL_HEADER_LEN + 4 {
            return SmallVec::new();
        }
        if Self::packet_type(buf) != Some(SRT_TYPE_NAK) {
            return SmallVec::new();
        }
        let mut out: SmallVec<u32, 8> = SmallVec::new();
        let mut i = SRT_CONTROL_HEADER_LEN;
        while i + 3 < buf.len() {
            let id = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
            i += 4;
            if (id & SRT_NAK_RANGE_FLAG) == 0 {
                out.push(id);
                continue;
            }

            let start = id & !SRT_NAK_RANGE_FLAG;
            if i + 3 >= buf.len() {
                break;
            }
            let end = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
            i += 4;

            // The end word is a plain sequence number, so its MSB must be clear;
            // anything else is corrupt or hostile. Combined with `end >= start`,
            // this also makes the walk below wrap-safe: `seq` is compared against
            // `end` *after* the push and before the increment, so it stops at `end`
            // and can never step past `0x7fff_ffff` into an overflow.
            if (end & SRT_NAK_RANGE_FLAG) != 0 || end < start {
                continue;
            }

            let mut seq = start;
            loop {
                if out.len() >= SRT_NAK_MAX_LOSS_IDS {
                    break;
                }
                out.push(seq);
                if seq == end {
                    break;
                }
                seq += 1;
            }
        }
        out
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
    pub fn parse_handshake_latency(buf: &[u8]) -> Option<SrtHandshakeLatency> {
        if Self::packet_type(buf)? != SRT_TYPE_HANDSHAKE {
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
                    snd_ms: ((flags & SRT_HS_OPT_TSBPDSND) != 0)
                        .then_some((latency & 0xffff) as u16),
                });
            }

            rest = &rest[4 + block_len..];
        }
        None
    }

    /// The initial sequence number of a new SRT session, when `buf` is the
    /// caller's induction handshake that opens it.
    ///
    /// The version word is not checked because an HSv5 caller still sends its
    /// induction as version 4. The ISN is masked to the 31 bits SRT sequences use.
    pub fn parse_session_start(buf: &[u8]) -> Option<u32> {
        if Self::packet_type(buf)? != SRT_TYPE_HANDSHAKE {
            return None;
        }
        let body = buf.get(SRT_CONTROL_HEADER_LEN..)?;
        if body.len() < SRT_HANDSHAKE_CIF_LEN {
            return None;
        }
        // Word 5 is the request type, word 2 the ISN.
        if be32(body, 20)? as i32 != SRT_HS_REQTYPE_INDUCTION {
            return None;
        }
        Some(be32(body, 8)? & 0x7fff_ffff)
    }
}

#[cfg(test)]
mod nak_tests {
    use super::*;

    /// Build a NAK frame whose CIF is `words`, laid out after the full 16-byte
    /// SRT control header.
    fn nak(words: &[u32]) -> Vec<u8> {
        let mut buf = vec![0u8; SRT_CONTROL_HEADER_LEN];
        buf[0..2].copy_from_slice(&SRT_TYPE_NAK.to_be_bytes());
        for w in words {
            buf.extend_from_slice(&w.to_be_bytes());
        }
        buf
    }

    #[test]
    fn single_entries_are_unchanged() {
        assert_eq!(ControlPacket::parse_srt_nak(&nak(&[7])).as_slice(), &[7]);
        assert_eq!(
            ControlPacket::parse_srt_nak(&nak(&[500, 501, 9])).as_slice(),
            &[500, 501, 9]
        );
    }

    #[test]
    fn ranges_are_unchanged() {
        assert_eq!(
            ControlPacket::parse_srt_nak(&nak(&[100 | SRT_NAK_RANGE_FLAG, 103])).as_slice(),
            &[100, 101, 102, 103]
        );
        // A single-element range: start == end.
        assert_eq!(
            ControlPacket::parse_srt_nak(&nak(&[42 | SRT_NAK_RANGE_FLAG, 42])).as_slice(),
            &[42]
        );
        // Mixed singles and ranges keep their wire order.
        assert_eq!(
            ControlPacket::parse_srt_nak(&nak(&[50, 100 | SRT_NAK_RANGE_FLAG, 102, 60])).as_slice(),
            &[50, 100, 101, 102, 60]
        );
    }

    #[test]
    fn end_word_with_msb_set_emits_nothing() {
        // 0xffff_ffff is the pathological case: unvalidated, `seq <= end` runs
        // to the cap and fabricates a thousand retransmission requests.
        assert!(
            ControlPacket::parse_srt_nak(&nak(&[10 | SRT_NAK_RANGE_FLAG, 0xffff_ffff])).is_empty()
        );
        // Any MSB-set end is rejected, not merely the all-ones one.
        assert!(
            ControlPacket::parse_srt_nak(&nak(&[10 | SRT_NAK_RANGE_FLAG, 20 | SRT_NAK_RANGE_FLAG]))
                .is_empty()
        );
    }

    #[test]
    fn invalid_range_does_not_desync_the_rest_of_the_list() {
        // The bad range is skipped; the entries after it still decode, which
        // means the two-word consumption stayed aligned.
        assert_eq!(
            ControlPacket::parse_srt_nak(&nak(&[1, 10 | SRT_NAK_RANGE_FLAG, 0xffff_ffff, 2]))
                .as_slice(),
            &[1, 2]
        );
    }

    #[test]
    fn end_below_start_yields_nothing() {
        assert!(ControlPacket::parse_srt_nak(&nak(&[100 | SRT_NAK_RANGE_FLAG, 99])).is_empty());
        assert_eq!(
            ControlPacket::parse_srt_nak(&nak(&[100 | SRT_NAK_RANGE_FLAG, 99, 7])).as_slice(),
            &[7]
        );
    }

    #[test]
    fn range_ending_at_max_sequence_terminates() {
        let max = 0x7fff_ffffu32;
        let out = ControlPacket::parse_srt_nak(&nak(&[(max - 3) | SRT_NAK_RANGE_FLAG, max]));
        assert_eq!(out.as_slice(), &[max - 3, max - 2, max - 1, max]);

        // Start == end == the largest legal sequence number: the walk must stop
        // on the first push rather than incrementing into an overflow.
        let out = ControlPacket::parse_srt_nak(&nak(&[max | SRT_NAK_RANGE_FLAG, max]));
        assert_eq!(out.as_slice(), &[max]);
    }

    #[test]
    fn oversized_range_saturates_at_the_cap() {
        let out = ControlPacket::parse_srt_nak(&nak(&[1 | SRT_NAK_RANGE_FLAG, 100_000]));
        assert_eq!(out.len(), SRT_NAK_MAX_LOSS_IDS);
        assert_eq!(out[0], 1);
        assert_eq!(out[SRT_NAK_MAX_LOSS_IDS - 1], SRT_NAK_MAX_LOSS_IDS as u32);

        // A range reaching the very top of the sequence space is bounded the
        // same way and, critically, still terminates.
        let out = ControlPacket::parse_srt_nak(&nak(&[SRT_NAK_RANGE_FLAG, 0x7fff_ffff]));
        assert_eq!(out.len(), SRT_NAK_MAX_LOSS_IDS);
    }

    #[test]
    fn range_start_word_without_an_end_word_is_dropped() {
        assert_eq!(
            ControlPacket::parse_srt_nak(&nak(&[7, 100 | SRT_NAK_RANGE_FLAG])).as_slice(),
            &[7]
        );
    }
}
