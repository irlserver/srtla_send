use super::constants::*;
use super::srt::ControlPacket;

/// Connection info data for extended keepalive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub conn_id: u32,
    pub window: i32,
    pub in_flight: i32,
    pub rtt_ms: u32,
    pub nak_count: u32,
    pub bitrate_bytes_per_sec: u32,
}

/// Wire codec for standard (10-byte) and extended (38-byte) SRTLA keepalive packets.
pub struct Keepalive;

impl Keepalive {
    /// `now` is the monotonic-ms timestamp stamped into the wire packet; the
    /// receiver echoes it back so the sender can measure keepalive RTT. Injected by
    /// the caller (this crate is I/O- and clock-free).
    pub fn encode(now: u64) -> [u8; 10] {
        let mut pkt = [0u8; 10];
        pkt[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
        pkt[2..10].copy_from_slice(&now.to_be_bytes());
        pkt
    }

    /// Create extended KEEPALIVE packet with connection info.
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
    pub fn encode_ext(info: ConnectionInfo, now: u64) -> [u8; SRTLA_KEEPALIVE_EXT_LEN] {
        let mut pkt = [0u8; SRTLA_KEEPALIVE_EXT_LEN];

        // Standard keepalive header (bytes 0-9). `now` is the echoed RTT timestamp.
        pkt[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
        pkt[2..10].copy_from_slice(&now.to_be_bytes());

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

    pub fn is_keepalive(buf: &[u8]) -> bool {
        ControlPacket::packet_type(buf) == Some(SRTLA_TYPE_KEEPALIVE)
    }

    pub fn timestamp(buf: &[u8]) -> Option<u64> {
        if buf.len() < 10 {
            return None;
        }
        if ControlPacket::packet_type(buf)? != SRTLA_TYPE_KEEPALIVE {
            return None;
        }
        let mut ts: u64 = 0;
        for i in 0..8 {
            ts = (ts << 8) | (buf[2 + i] as u64);
        }
        Some(ts)
    }

    /// Extract connection info from extended keepalive packet.
    ///
    /// Returns None if:
    /// - Packet is too short (< 38 bytes)
    /// - Not a KEEPALIVE packet
    /// - Magic number doesn't match (not an extended keepalive)
    /// - Version doesn't match
    pub fn conn_info(buf: &[u8]) -> Option<ConnectionInfo> {
        if buf.len() < SRTLA_KEEPALIVE_EXT_LEN {
            return None;
        }
        if ControlPacket::packet_type(buf)? != SRTLA_TYPE_KEEPALIVE {
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
}
