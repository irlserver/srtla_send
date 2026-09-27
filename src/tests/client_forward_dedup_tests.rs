//! SRTLA receivers send SRT ACKs, and some also NAKs, down every uplink of the
//! bond. The local SRT client must see each distinct packet once, whatever the
//! link count, while every copy still feeds the arrival link's accounting.
//!
//! These drive the real uplink receive path against a UDP socket standing in
//! for the SRT client and count what it receives.

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use smallvec::SmallVec;
    use srtla_core::connection::SrtlaConnection;
    use srtla_core::registration::SrtlaRegistrationManager;
    use srtla_core::test_helpers::create_test_connections;
    use srtla_protocol::*;
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    use crate::config::DynamicConfig;
    use crate::sender::{
        ClientDedup, ConnIoMap, SequenceTracker, UplinkPacket, handle_uplink_packet,
        process_uplink_packet,
    };

    type Instant = (SocketAddr, SmallVec<u8, 64>);

    /// A full SRT ACK: ACK number in the header, last acknowledged sequence as
    /// the first CIF word.
    fn full_ack(ack_number: u32, seq: u32) -> Vec<u8> {
        let mut pkt = SRT_TYPE_ACK.to_be_bytes().to_vec();
        pkt.resize(SRT_CONTROL_HEADER_LEN, 0);
        pkt[4..8].copy_from_slice(&ack_number.to_be_bytes());
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.resize(44, 0);
        pkt
    }

    fn nak(lost: u32) -> Vec<u8> {
        let mut pkt = SRT_TYPE_NAK.to_be_bytes().to_vec();
        pkt.resize(SRT_CONTROL_HEADER_LEN, 0);
        pkt.extend_from_slice(&lost.to_be_bytes());
        pkt
    }

    struct Bond {
        connections: SmallVec<SrtlaConnection, 4>,
        reg: SrtlaRegistrationManager,
        instant_tx: UnboundedSender<Instant>,
        instant_rx: UnboundedReceiver<Instant>,
        dedup: ClientDedup,
        listener: UdpSocket,
        client: UdpSocket,
        seq_tracker: SequenceTracker,
        config: DynamicConfig,
    }

    impl Bond {
        async fn new(links: usize) -> Self {
            let (instant_tx, instant_rx) = unbounded_channel();
            Self {
                connections: create_test_connections(links).await,
                reg: SrtlaRegistrationManager::new(),
                instant_tx,
                instant_rx,
                dedup: ClientDedup::new(),
                listener: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
                client: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
                seq_tracker: SequenceTracker::new(),
                config: DynamicConfig::new(),
            }
        }

        async fn receive_on(&mut self, link: usize, data: &[u8]) {
            let packet = UplinkPacket {
                conn_id: self.connections[link].conn_id,
                bytes: SmallVec::from_slice_copy(data),
            };
            let client_addr = self.client.local_addr().unwrap();
            handle_uplink_packet(
                packet,
                &mut self.connections,
                &ConnIoMap::new(),
                &mut self.reg,
                &self.instant_tx,
                &mut self.dedup,
                Some(client_addr),
                &self.listener,
                &self.seq_tracker,
                &self.config.snapshot(),
                &self.config,
            )
            .await;
        }

        /// Everything that reached the client, by socket or by the
        /// would-block channel.
        async fn delivered(&mut self) -> Vec<Vec<u8>> {
            let mut out = Vec::new();
            let mut buf = [0u8; 1500];
            while let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(50), self.client.recv(&mut buf)).await
            {
                out.push(buf[..n].to_vec());
            }
            while let Ok((_, pkt)) = self.instant_rx.try_recv() {
                out.push(pkt.to_vec());
            }
            out
        }
    }

    #[tokio::test]
    async fn one_ack_copy_reaches_the_client_once() {
        let mut bond = Bond::new(1).await;
        let ack = full_ack(1, 100);
        bond.receive_on(0, &ack).await;
        assert_eq!(bond.delivered().await, vec![ack]);
    }

    #[tokio::test]
    async fn an_ack_on_three_links_reaches_the_client_once() {
        let mut bond = Bond::new(3).await;
        let ack = full_ack(1, 100);
        for link in 0..3 {
            bond.receive_on(link, &ack).await;
        }
        assert_eq!(bond.delivered().await, vec![ack]);
    }

    #[tokio::test]
    async fn a_new_ack_is_forwarded() {
        let mut bond = Bond::new(3).await;
        let first = full_ack(1, 100);
        let second = full_ack(2, 100);
        for link in 0..3 {
            bond.receive_on(link, &first).await;
        }
        for link in 0..3 {
            bond.receive_on(link, &second).await;
        }
        assert_eq!(bond.delivered().await, vec![first, second]);
    }

    #[tokio::test]
    async fn a_nak_on_three_links_reaches_the_client_once() {
        let mut bond = Bond::new(3).await;
        let nak = nak(500);
        for link in 0..3 {
            bond.receive_on(link, &nak).await;
        }
        assert_eq!(bond.delivered().await, vec![nak]);
    }

    #[tokio::test]
    async fn every_copy_still_reaches_link_accounting() {
        let mut connections = create_test_connections(3).await;
        let mut reg = SrtlaRegistrationManager::new();
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (instant_tx, _instant_rx) = unbounded_channel();
        let mut dedup = ClientDedup::new();
        let ack = full_ack(1, 100);
        let nak = nak(500);

        for (link, conn) in connections.iter_mut().enumerate() {
            conn.last_received = None;
            for pkt in [&ack, &nak] {
                let incoming = process_uplink_packet(
                    conn,
                    link,
                    &mut reg,
                    &listener,
                    &instant_tx,
                    Some(client.local_addr().unwrap()),
                    &mut dedup,
                    pkt,
                )
                .await
                .unwrap();
                if pkt == &ack {
                    assert_eq!(incoming.ack_numbers.as_slice(), &[100]);
                } else {
                    assert_eq!(incoming.nak_numbers.as_slice(), &[500]);
                    assert_eq!(
                        incoming.forward_to_client.len(),
                        usize::from(link == 0),
                        "only the first NAK copy is queued for the client"
                    );
                }
            }
            assert!(
                conn.last_received.is_some(),
                "link {link}: a deduplicated copy still proves the link is alive"
            );
        }
    }
}
