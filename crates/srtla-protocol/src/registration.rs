use super::constants::*;
use super::srt::ControlPacket;

/// Wire codec for SRTLA registration frames (REG1, REG2, REG3).
pub struct RegPacket;

impl RegPacket {
    pub fn reg1(id: &[u8; SRTLA_ID_LEN]) -> [u8; SRTLA_TYPE_REG1_LEN] {
        let mut pkt = [0u8; SRTLA_TYPE_REG1_LEN];
        pkt[0..2].copy_from_slice(&SRTLA_TYPE_REG1.to_be_bytes());
        pkt[2..].copy_from_slice(id);
        pkt
    }

    pub fn reg2(id: &[u8; SRTLA_ID_LEN]) -> [u8; SRTLA_TYPE_REG2_LEN] {
        let mut pkt = [0u8; SRTLA_TYPE_REG2_LEN];
        pkt[0..2].copy_from_slice(&SRTLA_TYPE_REG2.to_be_bytes());
        pkt[2..].copy_from_slice(id);
        pkt
    }

    pub fn is_reg1(buf: &[u8]) -> bool {
        buf.len() == SRTLA_TYPE_REG1_LEN && ControlPacket::packet_type(buf) == Some(SRTLA_TYPE_REG1)
    }

    pub fn is_reg2(buf: &[u8]) -> bool {
        buf.len() == SRTLA_TYPE_REG2_LEN && ControlPacket::packet_type(buf) == Some(SRTLA_TYPE_REG2)
    }

    pub fn is_reg3(buf: &[u8]) -> bool {
        buf.len() == SRTLA_TYPE_REG3_LEN && ControlPacket::packet_type(buf) == Some(SRTLA_TYPE_REG3)
    }
}
