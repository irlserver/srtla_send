//! The receive path's SRT handshake sniff.
//!
//! The wire-format walk is unit-tested in `srtla-protocol`; what matters here
//! is that the uplink receive path reaches it for the right packets, keeps
//! relaying the handshake to the client either way, and hands the result to the
//! shared config where the delay budget is read from.

#[cfg(test)]
mod tests {
    use srtla_core::registration::SrtlaRegistrationManager;
    use srtla_core::test_helpers::create_test_connections;
    use srtla_protocol::*;

    use crate::config::DynamicConfig;
    use crate::sender::process_uplink_packet;

    const BOTH_TSBPD: u32 = SRT_HS_OPT_TSBPDSND | SRT_HS_OPT_TSBPDRCV;

    /// An HSv5 conclusion handshake carrying one HSREQ or HSRSP block.
    fn conclusion_with(cmd: u16, rcv_ms: u16, snd_ms: u16) -> Vec<u8> {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&SRT_TYPE_HANDSHAKE.to_be_bytes());
        pkt.extend_from_slice(&[0u8; SRT_CONTROL_HEADER_LEN - 2]);

        let mut cif = [0u32; SRT_HANDSHAKE_CIF_LEN / 4];
        cif[0] = SRT_HS_VERSION_5;
        cif[1] = SRT_HS_EXT_FLAG_HSREQ;
        cif[5] = SRT_HS_REQTYPE_CONCLUSION as u32;
        for word in cif {
            pkt.extend_from_slice(&word.to_be_bytes());
        }

        let body: [u32; 3] = [
            0x0001_0500,
            BOTH_TSBPD,
            ((rcv_ms as u32) << 16) | snd_ms as u32,
        ];
        pkt.extend_from_slice(&(((cmd as u32) << 16) | body.len() as u32).to_be_bytes());
        for word in body {
            pkt.extend_from_slice(&word.to_be_bytes());
        }
        pkt
    }

    /// Drive one datagram through the real uplink receive path and report the
    /// latency it surfaced plus how many packets it relayed downstream.
    async fn receive(data: &[u8]) -> (Option<u16>, usize) {
        let mut conns = create_test_connections(1).await;
        let mut reg = SrtlaRegistrationManager::new();
        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (instant_tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let incoming = process_uplink_packet(
            &mut conns[0],
            0,
            &mut reg,
            &listener,
            &instant_tx,
            None,
            data,
        )
        .await
        .unwrap();

        (
            incoming.negotiated_latency_ms,
            incoming.forward_to_client.len(),
        )
    }

    #[tokio::test]
    async fn an_hsrsp_surfaces_the_peers_receive_buffer() {
        let (latency, forwarded) = receive(&conclusion_with(SRT_HS_EXT_CMD_HSRSP, 4000, 120)).await;
        assert_eq!(latency, Some(4000));
        assert_eq!(
            forwarded, 1,
            "sniffing must not consume the handshake — it is still the client's"
        );
    }

    #[tokio::test]
    async fn an_hsreq_is_ignored_but_still_relayed() {
        // An HSREQ arriving from upstream is a proposal, not the negotiated
        // result: only the responder has resolved both sides to max(own,
        // proposed). Adopting it would let the far end's *ask* set our budget.
        let (latency, forwarded) = receive(&conclusion_with(SRT_HS_EXT_CMD_HSREQ, 4000, 120)).await;
        assert_eq!(latency, None);
        assert_eq!(forwarded, 1);
    }

    #[tokio::test]
    async fn a_malformed_handshake_is_relayed_untouched() {
        // Truncated mid-block. The client's SRT stack is the authority on
        // whether this is usable; our sniff must neither panic nor drop it.
        let full = conclusion_with(SRT_HS_EXT_CMD_HSRSP, 4000, 120);
        let (latency, forwarded) = receive(&full[..full.len() - 6]).await;
        assert_eq!(latency, None);
        assert_eq!(forwarded, 1);
    }

    #[test]
    fn the_budget_is_stored_once_and_reported_only_on_change() {
        // The handshake crosses once per session, but a re-handshake on
        // different terms must move the budget rather than being ignored.
        let config = DynamicConfig::new();
        assert_eq!(
            config.snapshot().negotiated_latency_ms,
            0,
            "unknown at start"
        );

        assert!(config.set_negotiated_latency_ms(4000));
        assert_eq!(config.snapshot().negotiated_latency_ms, 4000);

        assert!(
            !config.set_negotiated_latency_ms(4000),
            "a repeat of the same value is not a change"
        );

        assert!(
            config.set_negotiated_latency_ms(2000),
            "a re-handshake moves it"
        );
        assert_eq!(config.snapshot().negotiated_latency_ms, 2000);
    }
}
