use smallvec::SmallVec;

use crate::protocol::SRTLA_TYPE_REG1_LEN;

#[derive(Default)]
pub struct SrtlaIncoming {
    pub forward_to_client: SmallVec<SmallVec<u8, 64>, 4>,
    pub ack_numbers: SmallVec<u32, 4>,
    pub nak_numbers: SmallVec<u32, 4>,
    pub srtla_ack_numbers: SmallVec<u32, 4>,
    pub read_any: bool,
    /// REG1 to transmit on THIS connection's socket, produced when a REG_NGP
    /// arrived and the registration driver wants an immediate answer. The shell
    /// sends it after `process_packet` returns, keeping the receive path free of
    /// uplink I/O.
    pub reg1_send: Option<[u8; SRTLA_TYPE_REG1_LEN]>,
}
