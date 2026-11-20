//! Per-link arrival reports: how many sequence entries the receiver listed in
//! SRTLA ACKs on each link. The receiver sends an SRTLA ACK only on the link
//! the packets arrived on, so the entries belong to the arrival link, whatever
//! the sender's packet logs still hold.

#[cfg(test)]
mod tests {
    use srtla_core::connection::{SrtlaConnection, SrtlaIncoming};
    use srtla_core::utils::now_ms;

    use crate::config::ConfigSnapshot;
    use crate::sender::{SequenceTracker, process_connection_events};
    use crate::stats::SharedStats;
    use crate::test_helpers::create_test_connections;

    async fn ack_on(conns: &mut [SrtlaConnection], link: usize, seqs: &[u32]) {
        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seq_tracker = SequenceTracker::new();
        let incoming = SrtlaIncoming {
            srtla_ack_numbers: seqs.iter().copied().collect(),
            read_any: true,
            ..Default::default()
        };
        process_connection_events(link, conns, None, &listener, &seq_tracker, false, incoming)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn entries_count_for_the_link_the_ack_arrived_on() {
        let mut conns = create_test_connections(2).await;
        let now = now_ms();
        // Link 0 sent these; a NAK and a resend could have moved or cleared
        // them since. The receiver reports them as arrived on link 1.
        for s in [10, 11, 12] {
            conns[0].register_packet(s, now);
        }
        ack_on(&mut conns, 1, &[10, 11, 12]).await;
        assert_eq!(conns[1].srtla_ack_entries_total(), 3);
        assert_eq!(conns[0].srtla_ack_entries_total(), 0);
        assert!(conns[1].last_srtla_ack_age_ms(now_ms()).is_some());
        assert_eq!(conns[0].last_srtla_ack_age_ms(now_ms()), None);
    }

    #[tokio::test]
    async fn entries_no_log_holds_still_count() {
        let mut conns = create_test_connections(2).await;
        ack_on(&mut conns, 0, &[1, 2, 3, 4, 5]).await;
        ack_on(&mut conns, 0, &[6, 7]).await;
        assert_eq!(conns[0].srtla_ack_entries_total(), 7);
    }

    #[tokio::test]
    async fn nothing_counts_before_registration() {
        let mut conns = create_test_connections(1).await;
        conns[0].connected = false;
        ack_on(&mut conns, 0, &[1, 2, 3]).await;
        assert_eq!(conns[0].srtla_ack_entries_total(), 0);
        assert_eq!(conns[0].last_srtla_ack_age_ms(now_ms()), None);
    }

    #[tokio::test]
    async fn only_srt_data_counts_as_queued() {
        let mut conns = create_test_connections(1).await;
        let now = now_ms();
        let c = &mut conns[0];
        c.queue_data_packet(&[0u8; 1316], Some(1), now);
        // Forwarded SRT control (no sequence) is not data.
        c.queue_data_packet(&[0u8; 64], None, now);
        c.queue_probe_packet(&[0u8; 1316], 3, now);
        assert_eq!(c.data_packets_queued_total(), 2);
    }

    #[tokio::test]
    async fn a_socket_replacement_starts_a_new_epoch_and_clears_the_age() {
        let mut conns = create_test_connections(1).await;
        let now = now_ms();
        conns[0].note_srtla_ack_arrival(10, now);
        let before = conns[0].srtla_ack_epoch();
        conns[0].reset_for_reconnect(now + 1);
        assert_ne!(conns[0].srtla_ack_epoch(), before);
        assert_eq!(conns[0].last_srtla_ack_age_ms(now + 2), None);
        // The totals belong to the connection object and survive.
        assert_eq!(conns[0].srtla_ack_entries_total(), 10);
    }

    #[tokio::test]
    async fn a_link_created_again_never_repeats_an_epoch() {
        let first = create_test_connections(1).await;
        let again = create_test_connections(1).await;
        assert_ne!(first[0].srtla_ack_epoch(), again[0].srtla_ack_epoch());
        assert_eq!(again[0].srtla_ack_entries_total(), 0);
    }

    #[tokio::test]
    async fn an_ack_queued_by_a_retired_socket_never_counts_in_the_new_epoch() {
        use std::net::SocketAddr;

        use smallvec::SmallVec;
        use srtla_core::registration::SrtlaRegistrationManager;
        use srtla_core::test_helpers::create_test_connection;
        use srtla_protocol::create_ack_packet;
        use tokio::net::UdpSocket;
        use tokio::sync::mpsc::unbounded_channel;

        use crate::config::DynamicConfig;
        use crate::sender::{ClientDedup, UplinkPacket, handle_uplink_packet};
        use crate::test_helpers::create_test_conn_io_map;

        let mut connections = vec![create_test_connection().await];
        let conn_id = connections[0].conn_id;
        // Two socket instances for the same link: the first is the one a
        // rebuild retired, the second is the link's socket now.
        let retired = create_test_conn_io_map(&connections)[&conn_id]
            .socket
            .generation();
        let conn_io = create_test_conn_io_map(&connections);
        let live = conn_io[&conn_id].socket.generation();
        assert_ne!(retired, live);
        connections[0].reset_for_reconnect(now_ms());
        connections[0].connected = true;
        let epoch = connections[0].srtla_ack_epoch();

        let mut reg = SrtlaRegistrationManager::new();
        let (instant_tx, _instant_rx) = unbounded_channel::<(SocketAddr, SmallVec<u8, 64>)>();
        let mut dedup = ClientDedup::new();
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seq_tracker = SequenceTracker::new();
        let config = DynamicConfig::new();
        let acks: Vec<u32> = (1..=10).collect();

        for (generation, entries) in [(retired, 0), (live, 10)] {
            let packet = UplinkPacket {
                conn_id,
                generation,
                bytes: create_ack_packet(&acks),
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
                connections[0].srtla_ack_entries_total(),
                entries,
                "generation {generation} (live socket {live})"
            );
            assert_eq!(
                connections[0].last_srtla_ack_age_ms(now_ms()).is_some(),
                entries > 0,
                "the age stays absent until the live socket reports"
            );
        }
        assert_eq!(connections[0].srtla_ack_epoch(), epoch);
    }

    #[tokio::test]
    async fn get_stats_and_metrics_report_the_arrival_counters() {
        let mut conns = create_test_connections(2).await;
        let now = now_ms();
        conns[0].queue_data_packet(&[0u8; 1316], Some(1), now);
        ack_on(&mut conns, 0, &[1]).await;
        let stats = SharedStats::new();
        stats.update(&conns, &ConfigSnapshot::default(), None, None);
        let v: serde_json::Value = serde_json::from_str(&stats.to_json()).unwrap();
        let links = v["links"].as_array().unwrap();
        assert_eq!(links[0]["data_packets_queued_total"], 1);
        assert_eq!(links[0]["srtla_ack_entries_total"], 1);
        assert!(links[0]["last_srtla_ack_age_ms"].is_u64());
        assert_eq!(links[0]["srtla_ack_epoch"], conns[0].srtla_ack_epoch());
        assert!(
            links[1].get("last_srtla_ack_age_ms").is_none(),
            "absent before the first ACK, never null: {}",
            links[1]
        );
        let text = crate::metrics::render(
            &stats,
            &crate::config::DynamicConfig::new(),
            &srtla_core::priority::CriticalWindow::new(),
        );
        assert!(
            text.contains(r#"srtla_send_link_srtla_ack_entries_total{ip="192.168.1.10"} 1"#),
            "{text}"
        );
        assert!(text.contains(r#"srtla_send_link_data_packets_queued_total{ip="192.168.1.10"} 1"#));
    }
}
