//! The encoder opening a new SRT session resets every link's cumulative ACK
//! state, so a new ISN behind the old session's highest ACK cannot freeze
//! ACK processing.

#[cfg(test)]
mod tests {
    use srtla_core::seq::NO_ACK_YET;

    use crate::sender::note_srt_session_start;
    use crate::test_helpers::create_test_connections;

    /// A caller induction as libsrt sends it, built by hand because the
    /// protocol crate's handshake builder is private to its tests.
    fn caller_induction(isn: u32) -> Vec<u8> {
        let mut pkt = vec![0u8; 64];
        pkt[0..2].copy_from_slice(&srtla_protocol::SRT_TYPE_HANDSHAKE.to_be_bytes());
        pkt[16..20].copy_from_slice(&4u32.to_be_bytes());
        pkt[24..28].copy_from_slice(&isn.to_be_bytes());
        pkt[36..40].copy_from_slice(&1u32.to_be_bytes());
        pkt
    }

    #[tokio::test]
    async fn new_srt_session_resets_every_link() {
        let mut connections = create_test_connections(3).await;
        for conn in connections.iter_mut() {
            conn.register_packet(10, 1_000);
            conn.handle_srt_ack(10, 1_040, false);
            assert_eq!(conn.highest_acked_seq, 10);
            conn.register_packet(11, 1_050);
            assert_eq!(conn.in_flight_packets, 1);
        }

        let pkt = caller_induction(0x0abc_def0);
        assert_eq!(
            srtla_protocol::ControlPacket::parse_session_start(&pkt),
            Some(0x0abc_def0)
        );

        let mut last = None;
        assert!(note_srt_session_start(
            0x0abc_def0,
            &mut connections,
            &mut last
        ));
        for conn in &connections {
            assert_eq!(conn.highest_acked_seq, NO_ACK_YET);
            assert!(conn.packet_log.is_empty());
            assert_eq!(conn.in_flight_packets, 0);
        }
        assert_eq!(last, Some(0x0abc_def0));

        // The encoder resends its induction until answered. A repeat of the
        // same ISN is the same session.
        assert!(!note_srt_session_start(
            0x0abc_def0,
            &mut connections,
            &mut last
        ));
    }
}
