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
#[allow(dead_code)]
pub const SRT_TYPE_HANDSHAKE: u16 = 0x8000;
pub const SRT_TYPE_ACK: u16 = 0x8002;
pub const SRT_TYPE_NAK: u16 = 0x8003;
#[allow(dead_code)]
pub const SRT_TYPE_SHUTDOWN: u16 = 0x8005;
#[allow(dead_code)]
pub const SRT_TYPE_DATA: u16 = 0x0000;

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
