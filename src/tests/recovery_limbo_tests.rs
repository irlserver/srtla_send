//! A link that was established and then put into recovery must stay on the
//! timeout-driven reconnect path until REG3 re-admits it.
//!
//! Recovery (`mark_for_recovery`) clears `connected`, so the scheduler stops
//! sending data on the link, and relies on `is_timed_out` to route it through
//! housekeeping's socket rebuild + REG2. If the interface comes back quickly
//! (a replugged tether keeps its address), the receiver's traffic — SRT
//! ACK/NAKs, then replies to the keepalives housekeeping sends to a link it
//! believes alive — used to refresh `last_received`. The link then never timed
//! out, was never re-registered, and carried no data until the receiver gave up
//! on it (~15-20 s on the wire), long after the SRT peer-idle timeout.

#[cfg(test)]
mod tests {
    use srtla_core::registration::SrtlaRegistrationManager;
    use srtla_core::test_helpers::create_test_connections;
    use srtla_core::utils::now_ms;
    use srtla_protocol::*;

    use crate::sender::process_uplink_packet;

    async fn deliver(conn: &mut srtla_core::connection::SrtlaConnection, data: &[u8]) {
        let mut reg = SrtlaRegistrationManager::new();
        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (instant_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        process_uplink_packet(conn, 0, &mut reg, &listener, &instant_tx, None, data)
            .await
            .unwrap();
    }

    fn srt_ack() -> Vec<u8> {
        let mut pkt = SRT_TYPE_ACK.to_be_bytes().to_vec();
        pkt.resize(44, 0);
        pkt
    }

    async fn recovering_link() -> srtla_core::connection::SrtlaConnection {
        let mut conns = create_test_connections(1).await;
        let mut conn = conns.remove(0);
        conn.connected = true;
        conn.reconnection.connection_established_ms = 1;
        conn.last_received = Some(now_ms());
        conn.mark_for_recovery();
        assert!(!conn.connected);
        assert!(
            conn.is_timed_out(now_ms()),
            "recovery must time the link out"
        );
        conn
    }

    #[tokio::test]
    async fn receiver_traffic_does_not_rescue_a_recovering_link() {
        let mut conn = recovering_link().await;
        deliver(&mut conn, &srt_ack()).await;
        assert!(
            conn.is_timed_out(now_ms()),
            "an SRT ACK on a link awaiting REG3 must not take it off the reconnect path"
        );
    }

    #[tokio::test]
    async fn keepalive_reply_does_not_rescue_a_recovering_link() {
        let mut conn = recovering_link().await;
        deliver(&mut conn, &create_keepalive_packet(now_ms())).await;
        assert!(conn.is_timed_out(now_ms()));
    }

    #[tokio::test]
    async fn traffic_still_refreshes_a_connected_link() {
        let mut conns = create_test_connections(1).await;
        let conn = &mut conns[0];
        conn.connected = true;
        conn.reconnection.connection_established_ms = 1;
        conn.last_received = Some(now_ms().saturating_sub(1500));
        deliver(conn, &srt_ack()).await;
        assert!(conn.last_received.unwrap() >= now_ms().saturating_sub(10));
    }
}
