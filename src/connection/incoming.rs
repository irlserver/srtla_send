use smallvec::SmallVec;

#[derive(Default)]
pub struct SrtlaIncoming {
    pub forward_to_client: SmallVec<SmallVec<u8, 64>, 4>,
    pub ack_numbers: SmallVec<u32, 4>,
    pub nak_numbers: SmallVec<u32, 4>,
    pub srtla_ack_numbers: SmallVec<u32, 4>,
    pub read_any: bool,
}
