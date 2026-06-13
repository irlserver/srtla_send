// SRTLA protocol type constants
pub const SRTLA_TYPE_KEEPALIVE: u16 = 0x9000;
pub const SRTLA_TYPE_ACK: u16 = 0x9100;
pub const SRTLA_TYPE_REG1: u16 = 0x9200;
pub const SRTLA_TYPE_REG2: u16 = 0x9201;
pub const SRTLA_TYPE_REG3: u16 = 0x9202;
pub const SRTLA_TYPE_REG_ERR: u16 = 0x9210;
pub const SRTLA_TYPE_REG_NGP: u16 = 0x9211;
#[allow(dead_code)] // justified: reference constant — receiver never sends REG_NAK to sender per srtla protocol
pub const SRTLA_TYPE_REG_NAK: u16 = 0x9212;

// SRT protocol constants (some used in tests or for protocol completeness)
#[allow(dead_code)] // justified: used in integration_tests.rs for protocol validation
pub const SRT_TYPE_HANDSHAKE: u16 = 0x8000;
pub const SRT_TYPE_ACK: u16 = 0x8002;
pub const SRT_TYPE_NAK: u16 = 0x8003;
#[allow(dead_code)] // justified: used in integration_tests.rs for protocol validation
pub const SRT_TYPE_SHUTDOWN: u16 = 0x8005;
#[allow(dead_code)] // justified: used in integration_tests.rs for protocol validation
pub const SRT_TYPE_DATA: u16 = 0x0000;

// Packet size constants
pub const SRTLA_ID_LEN: usize = 256;
pub const SRTLA_TYPE_REG1_LEN: usize = 2 + SRTLA_ID_LEN;
pub const SRTLA_TYPE_REG2_LEN: usize = 2 + SRTLA_ID_LEN;
#[allow(dead_code)] // justified: used in integration_tests.rs for protocol validation
pub const SRTLA_TYPE_REG3_LEN: usize = 2;

pub const MTU: usize = 1500;

// Timeout constants
// WHY 15 (not the upstream 5): seconds of inbound silence before a link is
// declared failed. This MUST match the bonding receiver's CONN_TIMEOUT
// (srtla/src/receiver_config.h:28 = 15) and the C sender's SENDER_CONN_TIMEOUT
// (srtla/src/sender_logic.h:66 = 15). Below the receiver's 15 s window the
// receiver still holds the link and keeps echoing keepalives, so the two ends
// must agree on liveness: a sender that gives up sooner needlessly re-registers
// and resets the window to WINDOW_DEF on a link that is merely mid radio-stall
// (the "false link-down"). Real dead-link detection is NOT slowed by this — a
// hard send failure marks the link for recovery in <1 s (sender/packet_handler.rs
// flush_batch -> mark_for_recovery) and quality scoring deselects a struggling
// link in ~1 s. The upstream irlserver value of 5 s was inherited verbatim and
// never a deliberate CeraLive choice; T12 reconciled this drift to restore
// sender/receiver parity. Pinned by `conn_timeout_value_pinned` — do not let an
// upstream merge silently revert it. (See AGENTS.md PARITY CONTRACT.)
pub const CONN_TIMEOUT: u64 = 15; // sec — matches receiver CONN_TIMEOUT (parity)
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
