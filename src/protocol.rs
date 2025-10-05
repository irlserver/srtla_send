#![allow(dead_code)]

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

pub const CONN_TIMEOUT: u64 = 4; // sec
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

/// Reads the 16-bit big-endian packet type from the start of a buffer.
///
/// Returns `Some(type)` when the buffer contains at least two bytes and those bytes are interpreted
/// as a big-endian `u16`; returns `None` when the buffer is shorter than two bytes.
///
/// # Examples
///
/// ```
/// let buf = [0x12u8, 0x34, 0xFF];
/// assert_eq!(get_packet_type(&buf), Some(0x1234));
///
/// let short: [u8; 1] = [0x00];
/// assert_eq!(get_packet_type(&short), None);
/// ```
#[inline]
pub fn get_packet_type(buf: &[u8]) -> Option<u16> {
    if buf.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([buf[0], buf[1]]))
}

/// Parses an SRT sequence number from the first four bytes of a buffer when the most-significant bit is clear.
///
/// # Returns
///
/// `Some(sequence_number)` if `buf` contains at least four bytes and the parsed big-endian 32-bit value has its most-significant bit equal to 0, `None` otherwise.
///
/// # Examples
///
/// ```
/// let sn: u32 = 12345;
/// let mut buf = sn.to_be_bytes().to_vec();
/// // valid when MSB is clear
/// assert_eq!(crate::get_srt_sequence_number(&buf), Some(12345));
///
/// // value with MSB set is treated as invalid
/// let invalid = (0x8000_0000u32 | 1).to_be_bytes();
/// assert_eq!(crate::get_srt_sequence_number(&invalid), None);
/// ```
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

/// Constructs an SRTLA REG1 packet containing the given 256-byte ID.
///
/// The returned packet is exactly SRTLA_TYPE_REG1_LEN bytes long and encodes the REG1 type in the first two bytes followed by the 256-byte identifier.
///
/// # Examples
///
/// ```
/// let id = [0u8; SRTLA_ID_LEN];
/// let pkt = create_reg1_packet(&id);
/// assert_eq!(pkt.len(), SRTLA_TYPE_REG1_LEN);
/// assert_eq!(&pkt[2..], &id);
/// ```
pub fn create_reg1_packet(id: &[u8; SRTLA_ID_LEN]) -> [u8; SRTLA_TYPE_REG1_LEN] {
    let mut pkt = [0u8; SRTLA_TYPE_REG1_LEN];
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_REG1.to_be_bytes());
    pkt[2..].copy_from_slice(id);
    pkt
}

/// Constructs an SRTLA REG2 packet containing the REG2 type followed by the provided 256-byte ID.
///
/// The returned fixed-size array is exactly SRTLA_TYPE_REG2_LEN bytes long, with the first two bytes
/// set to the big-endian REG2 type and the remaining bytes set to `id`.
///
/// # Examples
///
/// ```
/// let id = [0x42u8; SRTLA_ID_LEN];
/// let pkt = create_reg2_packet(&id);
/// // First two bytes are the REG2 type
/// assert_eq!(&pkt[0..2], &SRTLA_TYPE_REG2.to_be_bytes());
/// // Remaining bytes equal the id
/// assert_eq!(&pkt[2..], &id);
/// ```
pub fn create_reg2_packet(id: &[u8; SRTLA_ID_LEN]) -> [u8; SRTLA_TYPE_REG2_LEN] {
    let mut pkt = [0u8; SRTLA_TYPE_REG2_LEN];
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_REG2.to_be_bytes());
    pkt[2..].copy_from_slice(id);
    pkt
}

/// Constructs an SRTLA KEEPALIVE packet containing the current UTC timestamp.
///
/// The packet is 10 bytes: the first two bytes are the KEEPALIVE type in big-endian order,
/// and the remaining eight bytes are the UTC timestamp in milliseconds encoded as a big-endian u64.
///
/// # Examples
///
/// ```
/// let pkt = create_keepalive_packet();
/// assert_eq!(&pkt[0..2], &SRTLA_TYPE_KEEPALIVE.to_be_bytes());
/// let ts = u64::from_be_bytes(pkt[2..10].try_into().unwrap());
/// assert!(ts > 0);
/// ```
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

/// Builds an SRTLA-format ACK packet containing the provided acknowledgment sequence numbers.
///
/// The packet uses a 4-byte header (2-byte SRTLA ACK type in big-endian followed by two zero bytes)
/// followed by each acknowledgment encoded as a 4-byte big-endian `u32` in sequence.
///
/// # Parameters
///
/// - `acks`: slice of acknowledgment sequence numbers to include in the packet.
///
/// # Returns
///
/// A `Vec<u8>` containing the serialized ACK packet bytes.
///
/// # Examples
///
/// ```
/// let acks = [1u32, 0xDEADBEEFu32];
/// let pkt = create_ack_packet(&acks);
/// assert_eq!(pkt.len(), 4 + 4 * acks.len());
/// assert_eq!(&pkt[0..2], &SRTLA_TYPE_ACK.to_be_bytes());
/// assert_eq!(&pkt[2..4], &[0x00, 0x00]);
/// assert_eq!(&pkt[4..8], &acks[0].to_be_bytes());
/// assert_eq!(&pkt[8..12], &acks[1].to_be_bytes());
/// ```
pub fn create_ack_packet(acks: &[u32]) -> Vec<u8> {
    // Create packets that match the actual SRTLA receiver format (4-byte header)
    let mut pkt = vec![0u8; 4 + 4 * acks.len()];
    pkt[0..2].copy_from_slice(&SRTLA_TYPE_ACK.to_be_bytes());
    pkt[2] = 0x00; // Padding (matching receiver behavior)
    pkt[3] = 0x00; // Padding (matching receiver behavior)
    for (i, &ack) in acks.iter().enumerate() {
        let off = 4 + i * 4;
        pkt[off..off + 4].copy_from_slice(&ack.to_be_bytes());
    }
    pkt
}

/// Parses an SRT ACK packet and returns the acknowledged sequence number.
///
/// Returns `Some(sequence_number)` if `buf` is at least 20 bytes long and its packet type is SRT ACK;
/// returns `None` otherwise.
///
/// # Examples
///
/// ```rust,no_run
/// // Build a 20-byte buffer matching the SRT ACK layout (two-byte type, header, then sequence at bytes 16..20)
/// let mut buf = [0u8; 20];
/// // set packet type to the SRT ACK value expected by this crate (big-endian)
/// // buf[0..2] = SRT_TYPE_ACK.to_be_bytes();
/// let seq: u32 = 0x01020304;
/// buf[16..20].copy_from_slice(&seq.to_be_bytes());
///
/// assert_eq!(crate::parse_srt_ack(&buf), Some(seq));
/// ```
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

/// Parses an SRT NAK packet and returns the list of missing sequence numbers it encodes.
///
/// If `buf` is not a valid SRT NAK packet (length < 8 or wrong packet type), an empty vector is returned.
/// Single 32-bit sequence identifiers are appended directly. If an identifier has its high bit set,
/// it denotes an inclusive range: the high bit is cleared to obtain the start, the next 32-bit value is read
/// as the end, and every sequence number from start through end (wrapping with `wrapping_add`) is appended,
/// subject to a hard cap of 1000 entries. Malformed trailing/incomplete range pairs terminate parsing.
///
/// # Examples
///
/// ```
/// // single id
/// let mut buf = Vec::new();
/// buf.extend_from_slice(&SRT_TYPE_NAK.to_be_bytes()); // 2 bytes
/// buf.extend_from_slice(&0u16.to_be_bytes()); // 2-byte header padding
/// buf.extend_from_slice(&123u32.to_be_bytes());
/// assert_eq!(parse_srt_nak(&buf), vec![123]);
///
/// // range encoded with high bit set on start
/// let mut buf = Vec::new();
/// buf.extend_from_slice(&SRT_TYPE_NAK.to_be_bytes());
/// buf.extend_from_slice(&0u16.to_be_bytes());
/// let start = 1u32 | 0x8000_0000;
/// buf.extend_from_slice(&start.to_be_bytes());
/// buf.extend_from_slice(&3u32.to_be_bytes());
/// assert_eq!(parse_srt_nak(&buf), vec![1, 2, 3]);
/// ```
#[inline]
pub fn parse_srt_nak(buf: &[u8]) -> Vec<u32> {
    if buf.len() < 8 {
        return vec![];
    }
    if get_packet_type(buf) != Some(SRT_TYPE_NAK) {
        return vec![];
    }
    let mut out = Vec::new();
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

/// Parses an SRTLA ACK packet and returns the acknowledged sequence numbers in order.
///
/// The function returns an empty vector if `buf` is shorter than 8 bytes or if the packet
/// type in `buf` is not `SRTLA_TYPE_ACK`.
///
/// # Parameters
///
/// - `buf`: byte slice containing a candidate SRTLA packet.
///
/// # Returns
///
/// `Vec<u32>` containing each 32-bit acknowledgement value extracted from the packet; `vec![]`
/// if the buffer is too short or not an SRTLA ACK packet.
///
/// # Examples
///
/// ```
/// let mut buf = vec![0u8; 8];
/// buf[0..2].copy_from_slice(&SRTLA_TYPE_ACK.to_be_bytes());
/// // bytes 2..4 are padding; place one ack (1) at bytes 4..8
/// buf[4..8].copy_from_slice(&1u32.to_be_bytes());
/// assert_eq!(parse_srtla_ack(&buf), vec![1u32]);
/// ```
#[inline]
pub fn parse_srtla_ack(buf: &[u8]) -> Vec<u32> {
    if buf.len() < 8 {
        return vec![];
    }
    if get_packet_type(buf) != Some(SRTLA_TYPE_ACK) {
        return vec![];
    }
    let mut out = Vec::new();

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