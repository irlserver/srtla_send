//! A datagram queued by an uplink's previous socket must not count after the
//! socket was rebuilt, possibly toward another receiver address. The reader
//! tags every datagram with its socket's generation; the receive path drops
//! the ones from a retired socket.

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use smallvec::SmallVec;
    use srtla_core::registration::SrtlaRegistrationManager;
    use srtla_core::test_helpers::create_test_connection;
    use srtla_protocol::SRTLA_TYPE_REG3;
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc::unbounded_channel;

    use crate::config::DynamicConfig;
    use crate::sender::{ClientDedup, SequenceTracker, UplinkPacket, handle_uplink_packet};
    use crate::test_helpers::create_test_conn_io_map;

    #[tokio::test]
    async fn a_reg3_from_a_retired_socket_does_not_register_the_link() {
        let mut connections = vec![create_test_connection().await];
        connections[0].connected = false;
        let conn_id = connections[0].conn_id;
        let conn_io = create_test_conn_io_map(&connections);
        let live_generation = conn_io[&conn_id].socket.generation();
        let mut reg = SrtlaRegistrationManager::new();
        let (instant_tx, _instant_rx) = unbounded_channel::<(SocketAddr, SmallVec<u8, 64>)>();
        let mut dedup = ClientDedup::new();
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seq_tracker = SequenceTracker::new();
        let config = DynamicConfig::new();

        for (generation, registers) in [(live_generation + 1_000, false), (live_generation, true)] {
            // A REG2 went out on this link, so a REG3 for it is in phase.
            let _ = reg.build_reg2(conn_id);
            let packet = UplinkPacket {
                conn_id,
                generation,
                bytes: SmallVec::from_slice_copy(&SRTLA_TYPE_REG3.to_be_bytes()),
            };
            handle_uplink_packet(
                packet,
                &mut connections,
                &conn_io,
                &mut reg,
                &instant_tx,
                &mut dedup,
                None,
                &listener,
                &seq_tracker,
                &config.snapshot(),
                &config,
            )
            .await;
            assert_eq!(
                connections[0].connected, registers,
                "generation {generation} (live socket {live_generation})"
            );
        }
    }
}
