// SRTLA protocol type constants
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
pub const SRT_TYPE_HANDSHAKE: u16 = 0x8000;
pub const SRT_TYPE_ACK: u16 = 0x8002;
pub const SRT_TYPE_NAK: u16 = 0x8003;
#[allow(dead_code)]
pub const SRT_TYPE_SHUTDOWN: u16 = 0x8005;
#[allow(dead_code)]
pub const SRT_TYPE_DATA: u16 = 0x0000;

// SRT handshake layout, for reading the negotiated TSBPD latency off the wire
// (see `parse_srt_handshake_latency`). Field names and offsets follow libsrt's
// `CHandShake` / `SrtHSRequest`.

/// SRT control-packet header: `F|control type`, subtype, type-specific info,
/// timestamp, destination socket ID. The handshake body follows it.
pub const SRT_CONTROL_HEADER_LEN: usize = 16;

/// Length of the UDT handshake body (libsrt's `CHandShake::m_iContentSize`):
/// version, type, ISN, MSS, flight flag size, request type, socket ID, cookie,
/// and a 16-byte peer address. SRT extension blocks follow it.
pub const SRT_HANDSHAKE_CIF_LEN: usize = 48;

/// Handshake version that carries SRT extension blocks. HSv4 sends its SRT
/// handshake as a separate `UMSG_EXT` control packet instead, so extension
/// parsing only ever applies to version 5.
pub const SRT_HS_VERSION_5: u32 = 5;

/// `URQ_CONCLUSION`: the second handshake phase, the only one carrying
/// extensions. Induction (1), rendezvous agreement (-2) and the >= 1000
/// rejection codes all fail this check.
pub const SRT_HS_REQTYPE_CONCLUSION: i32 = -1;

/// `CHandShake::HS_EXT_HSREQ` — bit in the handshake's extension field saying
/// an HSREQ/HSRSP block is present.
pub const SRT_HS_EXT_FLAG_HSREQ: u32 = 1 << 0;

/// Extension-block commands we read. `SRT_CMD_HSREQ` is sent by the initiator,
/// `SRT_CMD_HSRSP` is the responder's answer and carries the *negotiated*
/// values.
pub const SRT_HS_EXT_CMD_HSREQ: u16 = 1;
pub const SRT_HS_EXT_CMD_HSRSP: u16 = 2;

/// Words in an HSREQ/HSRSP block: version, flags, latency (`SRT_HS_E_SIZE`).
pub const SRT_HS_EXT_HSREQ_WORDS: usize = 3;

/// TSBPD flags in the HSREQ/HSRSP flags word. Each says whether the
/// corresponding half of the latency word carries a real value.
pub const SRT_HS_OPT_TSBPDSND: u32 = 1 << 0;
pub const SRT_HS_OPT_TSBPDRCV: u32 = 1 << 1;

// Packet size constants
pub const SRTLA_ID_LEN: usize = 256;
pub const SRTLA_TYPE_REG1_LEN: usize = 2 + SRTLA_ID_LEN;
pub const SRTLA_TYPE_REG2_LEN: usize = 2 + SRTLA_ID_LEN;
#[allow(dead_code)]
pub const SRTLA_TYPE_REG3_LEN: usize = 2;

pub const MTU: usize = 1500;

// Timeout constants
pub const CONN_TIMEOUT: u64 = 5; // sec
pub const REG2_TIMEOUT: u64 = 4; // sec
pub const REG3_TIMEOUT: u64 = 4; // sec
pub const IDLE_TIME: u64 = 1; // sec

// Window management constants
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
