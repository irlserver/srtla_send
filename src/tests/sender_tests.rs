#[cfg(test)]
mod tests {

    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr};

    use smallvec::SmallVec;
    use tempfile::NamedTempFile;
    use tokio::time::Duration;

    use crate::config::{ConfigSnapshot, DynamicConfig};
    use crate::connection::SrtlaConnection;
    use crate::mode::SchedulingMode;
    use crate::protocol::{
        CONN_TIMEOUT, SRT_TYPE_ACK, SRT_TYPE_DATA, SRT_TYPE_NAK, get_packet_type, is_srt_ack,
    };
    use crate::sender::*;
    use crate::test_helpers::{advance_test_clock, create_test_connections};
    use crate::utils::now_ms;

    #[test]
    fn test_select_connection_idx_classic() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Test classic mode - should pick connection with highest score
        connections[1].in_flight_packets = 0; // Best score
        connections[0].in_flight_packets = 5; // Lower score
        connections[2].in_flight_packets = 10; // Lowest score

        let config = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: false,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };

        let selected = select_connection_idx(&mut connections, None, 0, 0, &config);
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn test_select_connection_idx_quality_scoring() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let current_time = now_ms();

        // Connection 0: Recent NAKs - should get low score
        connections[0].congestion.nak_count = 5;
        connections[0].congestion.last_nak_time_ms = current_time - 1000; // 1 second ago

        // Connection 1: No NAKs - should get bonus
        connections[1].congestion.nak_count = 0;

        // Connection 2: Old NAKs - should get partial penalty
        connections[2].congestion.nak_count = 3;
        connections[2].congestion.last_nak_time_ms = current_time - 8000; // 8 seconds ago

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };

        let selected = select_connection_idx(&mut connections, None, 0, current_time, &config);

        // Should prefer connection 1 (no NAKs)
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn test_select_connection_idx_burst_nak_penalty() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let current_time = now_ms();

        // Connection 0: NAK burst
        connections[0].congestion.nak_count = 5;
        connections[0].congestion.nak_burst_count = 3;
        connections[0].congestion.last_nak_time_ms = current_time - 2000; // 2 seconds ago

        // Connection 1: Same NAK count but no burst
        connections[1].congestion.nak_count = 5;
        connections[1].congestion.nak_burst_count = 0;
        connections[1].congestion.last_nak_time_ms = current_time - 2000; // 2 seconds ago

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };

        let selected = select_connection_idx(&mut connections, None, 0, current_time, &config);

        // Should prefer connection 2 (never had NAKs, best quality)
        assert_eq!(selected, Some(2));
    }

    #[test]
    fn test_time_based_switch_dampening_blocks_within_cooldown() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup: Connection 0 is currently selected, Connection 1 has better score
        connections[0].in_flight_packets = 5; // Lower score
        connections[1].in_flight_packets = 0; // Best score
        connections[2].in_flight_packets = 10; // Worst score

        let last_switch_time_ms = now_ms();
        let current_time_ms = last_switch_time_ms + 5; // 5ms after last switch (within 15ms cooldown)

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };

        // Per-packet selection: Should keep sending ALL packets via connection 0 during cooldown
        // This prevents rapid thrashing between connections under bursty score changes
        let selected = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            &config,
        );
        assert_eq!(
            selected,
            Some(0),
            "Should continue routing all packets via current connection during cooldown period"
        );
    }

    #[test]
    fn test_time_based_switch_dampening_allows_after_cooldown() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup: Connection 0 is currently selected, Connection 1 has significantly better score
        connections[0].in_flight_packets = 5; // Lower score
        connections[1].in_flight_packets = 0; // Best score (significantly better, exceeds 2% hysteresis)
        connections[2].in_flight_packets = 10; // Worst score

        let last_switch_time_ms = now_ms();
        let current_time_ms = last_switch_time_ms + 20; // 20ms after last switch (past 15ms cooldown)

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };

        // After cooldown: per-packet selection can now choose the better connection
        // From this point forward, all subsequent packets will route via connection 1
        let selected = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            &config,
        );
        assert_eq!(
            selected,
            Some(1),
            "Should switch per-packet routing to better connection after cooldown expires"
        );
    }

    #[test]
    fn test_time_based_switch_dampening_allows_if_current_invalid() {
        use tokio::time::{Duration, Instant};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup: Connection 0 is currently selected but becomes timed out
        connections[0].in_flight_packets = 5;
        // Simulate timeout by setting last_received past CONN_TIMEOUT
        connections[0].last_received = Some(Instant::now() - Duration::from_secs(CONN_TIMEOUT + 1));
        connections[1].in_flight_packets = 0; // Best score
        connections[2].in_flight_packets = 10;

        let last_switch_time_ms = now_ms();
        let current_time_ms = last_switch_time_ms + 5; // Within 15ms cooldown

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };

        // Cooldown is bypassed when current connection is invalid/timed out
        // Per-packet selection immediately switches to valid connection
        let selected = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            &config,
        );
        assert_eq!(
            selected,
            Some(1),
            "Should immediately route packets via valid connection if current is timed out, \
             bypassing cooldown"
        );
    }

    #[test]
    fn test_exploration_blocked_during_cooldown() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup connections with distinct scores
        connections[0].in_flight_packets = 2; // Currently selected
        connections[1].in_flight_packets = 0; // Best
        connections[2].in_flight_packets = 1; // Second-best

        let last_switch_time_ms = now_ms();
        let current_time_ms = last_switch_time_ms + 5; // Within 15ms cooldown

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: true, // exploration enabled
            rtt_delta_ms: 30,
        };

        // Enable exploration, but should be blocked by cooldown
        // This prevents exploration from causing rapid per-packet routing changes
        let selected = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            &config,
        );

        // Should continue routing packets via connection 0, not explore during cooldown
        assert_eq!(
            selected,
            Some(0),
            "Exploration-triggered per-packet routing changes should be blocked during cooldown"
        );
    }

    #[test]
    fn test_classic_mode_ignores_time_dampening() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup: Connection 0 is currently selected, Connection 1 has better score
        connections[0].in_flight_packets = 5; // Lower score
        connections[1].in_flight_packets = 0; // Best score
        connections[2].in_flight_packets = 10; // Worst score

        let last_switch_time_ms = now_ms();
        let current_time_ms = last_switch_time_ms + 200; // 200ms after last switch (within cooldown)

        let config = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: false,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };

        // Classic mode: per-packet selection ALWAYS picks highest score connection
        // No dampening, no hysteresis - matches original C implementation
        let selected = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            &config,
        );

        // Per-packet routing immediately uses connection 1 (best score)
        assert_eq!(
            selected,
            Some(1),
            "Classic mode per-packet selection should ignore time-based dampening and always \
             route via highest score connection"
        );
    }

    #[test]
    fn test_nak_attribution_to_correct_connection() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        let current_time = now_ms();
        connections[0].register_packet(100, current_time);
        connections[1].register_packet(200, current_time);
        connections[2].register_packet(300, current_time);

        let initial_counts = [
            connections[0].congestion.nak_count,
            connections[1].congestion.nak_count,
            connections[2].congestion.nak_count,
        ];

        let found_0 = connections[0].handle_nak(100);
        assert!(found_0);
        assert_eq!(connections[0].congestion.nak_count, initial_counts[0] + 1);
        assert_eq!(connections[1].congestion.nak_count, initial_counts[1]);
        assert_eq!(connections[2].congestion.nak_count, initial_counts[2]);

        let found_1 = connections[1].handle_nak(200);
        assert!(found_1);
        assert_eq!(connections[0].congestion.nak_count, initial_counts[0] + 1);
        assert_eq!(connections[1].congestion.nak_count, initial_counts[1] + 1);
        assert_eq!(connections[2].congestion.nak_count, initial_counts[2]);

        let not_found_0 = connections[0].handle_nak(999);
        let not_found_1 = connections[1].handle_nak(999);
        let not_found_2 = connections[2].handle_nak(999);
        assert!(!not_found_0);
        assert!(!not_found_1);
        assert!(!not_found_2);
        assert_eq!(connections[0].congestion.nak_count, initial_counts[0] + 1);
        assert_eq!(connections[1].congestion.nak_count, initial_counts[1] + 1);
        assert_eq!(connections[2].congestion.nak_count, initial_counts[2]);
    }

    #[tokio::test]
    async fn test_read_ip_list() {
        let mut temp_file = NamedTempFile::new().unwrap();
        writeln!(temp_file, "192.168.1.1").unwrap();
        writeln!(temp_file, "192.168.1.2").unwrap();
        writeln!(temp_file).unwrap(); // Empty line
        writeln!(temp_file, "192.168.1.3").unwrap();
        writeln!(temp_file, "invalid-ip").unwrap(); // Invalid IP

        let ips = read_ip_list(temp_file.path().to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(ips.len(), 3);
        assert_eq!(ips[0], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(ips[1], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)));
        assert_eq!(ips[2], IpAddr::V4(Ipv4Addr::new(192, 168, 1, 3)));
    }

    #[tokio::test]
    async fn test_read_ip_list_empty() {
        let temp_file = NamedTempFile::new().unwrap();

        let ips = read_ip_list(temp_file.path().to_str().unwrap())
            .await
            .unwrap();
        assert!(ips.is_empty());
    }

    #[tokio::test]
    async fn test_read_ip_list_nonexistent() {
        let result = read_ip_list("/nonexistent/file.txt").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_connection_changes_remove_stale() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let initial_count = connections.len();

        // New IPs that don't include all current connections
        let new_ips = vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), // Keep first connection
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), // New IP
        ];

        let mut last_selected_idx = Some(1);
        let mut seq_tracker = SequenceTracker::new();
        let now = now_ms();
        // Insert entries for connections that will be removed
        seq_tracker.insert(100, connections[1].conn_id, now);
        seq_tracker.insert(200, connections[2].conn_id, now);

        rt.block_on(apply_connection_changes(
            &mut connections,
            &new_ips,
            "127.0.0.1",
            8080,
            &mut last_selected_idx,
            &mut seq_tracker,
        ));

        // Should have removed some connections
        assert!(connections.len() < initial_count);

        // Should have reset selection
        assert_eq!(last_selected_idx, None);

        // Entries for removed connections should now return None
        // (they were cleaned up by remove_connection)
        assert!(seq_tracker.get(100, now).is_none());
        assert!(seq_tracker.get(200, now).is_none());
    }

    /// Relabel test connections as real `host:port via ip` uplinks so they match
    /// the labels `apply_connection_changes` derives, exercising the survivor path.
    fn relabel_as_uplinks(
        connections: &mut [SrtlaConnection],
        host: &str,
        port: u16,
        ips: &[IpAddr],
    ) {
        for (conn, ip) in connections.iter_mut().zip(ips.iter()) {
            conn.local_ip = *ip;
            conn.label = format!("{host}:{port} via {ip}");
        }
    }

    #[test]
    fn test_reload_reorders_conn_ids_to_file_order_keeping_survivors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));
        let host = "127.0.0.1";
        let port = 9000u16;
        let ip_a = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
        let ip_b = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3));
        relabel_as_uplinks(&mut connections, host, port, &[ip_a, ip_b]);
        let cid_a = connections[0].conn_id;
        let cid_b = connections[1].conn_id;

        let mut last_selected_idx = Some(0);
        let mut seq_tracker = SequenceTracker::new();

        // Reload reverses the file order: [B, A]. Both survive (no new binds).
        rt.block_on(apply_connection_changes(
            &mut connections,
            &[ip_b, ip_a],
            host,
            port,
            &mut last_selected_idx,
            &mut seq_tracker,
        ));

        assert_eq!(
            connections.len(),
            2,
            "both uplinks must survive the reorder"
        );
        assert_eq!(
            connections[0].conn_id, cid_b,
            "telemetry conn_id 0 must follow the new file order (B first)"
        );
        assert_eq!(connections[1].conn_id, cid_a, "conn_id 1 must be A");
        assert_eq!(
            last_selected_idx, None,
            "a reorder must invalidate the cached selection index"
        );
    }

    #[test]
    fn test_reload_removes_one_keeps_survivor_and_cleans_seq_tracker() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));
        let host = "127.0.0.1";
        let port = 9000u16;
        let ip_a = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
        let ip_b = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3));
        relabel_as_uplinks(&mut connections, host, port, &[ip_a, ip_b]);
        let cid_a = connections[0].conn_id;
        let cid_b = connections[1].conn_id;

        let mut last_selected_idx = Some(1);
        let mut seq_tracker = SequenceTracker::new();
        let now = now_ms();
        seq_tracker.insert(100, cid_b, now);

        // Reload drops B, keeps A.
        rt.block_on(apply_connection_changes(
            &mut connections,
            &[ip_a],
            host,
            port,
            &mut last_selected_idx,
            &mut seq_tracker,
        ));

        assert_eq!(connections.len(), 1, "only the surviving uplink remains");
        assert_eq!(
            connections[0].conn_id, cid_a,
            "survivor A keeps its identity (not torn down and rebuilt)"
        );
        assert!(
            seq_tracker.get(100, now).is_none(),
            "the removed uplink's sequence entry must be cleared"
        );
        assert_eq!(last_selected_idx, None);
    }

    #[test]
    fn test_reload_identical_list_keeps_selection_no_disconnect() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));
        let host = "127.0.0.1";
        let port = 9000u16;
        let ip_a = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
        let ip_b = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3));
        relabel_as_uplinks(&mut connections, host, port, &[ip_a, ip_b]);
        let cid_a = connections[0].conn_id;
        let cid_b = connections[1].conn_id;

        let mut last_selected_idx = Some(1);
        let mut seq_tracker = SequenceTracker::new();

        // A SIGHUP whose file is unchanged must not disturb any link.
        rt.block_on(apply_connection_changes(
            &mut connections,
            &[ip_a, ip_b],
            host,
            port,
            &mut last_selected_idx,
            &mut seq_tracker,
        ));

        assert_eq!(connections.len(), 2);
        assert_eq!(connections[0].conn_id, cid_a);
        assert_eq!(connections[1].conn_id, cid_b);
        assert_eq!(
            last_selected_idx,
            Some(1),
            "an unchanged reload must keep the selection index (zero disconnects)"
        );
    }

    #[test]
    fn test_pending_connection_changes() {
        let changes = PendingConnectionChanges {
            new_ips: Some(SmallVec::from_vec(vec![IpAddr::V4(Ipv4Addr::new(
                192, 168, 1, 100,
            ))])),
            receiver_host: "test-host".to_string(),
            receiver_port: 9090,
        };

        assert!(changes.new_ips.is_some());
        assert_eq!(changes.receiver_host, "test-host");
        assert_eq!(changes.receiver_port, 9090);
    }

    #[test]
    fn test_constants() {
        assert!(SEQ_TRACKING_SIZE > 0);
        assert!(GLOBAL_TIMEOUT_MS > 0);

        // Should handle decent throughput (16384 entries)
        assert!(SEQ_TRACKING_SIZE >= 1000);
        // Should allow time for connections
        assert!(GLOBAL_TIMEOUT_MS >= 5000);
    }

    #[tokio::test]
    async fn test_create_connections_from_ips() {
        let ips = vec![
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        ];

        // This will likely fail to connect but should not panic
        let connections = create_connections_from_ips(&ips, "127.0.0.1", 9999).await;

        // Connections may be empty due to connection failures, which is OK for testing
        assert!(connections.len() <= ips.len());
    }

    #[test]
    fn test_sequence_tracking_limits() {
        let mut seq_tracker = SequenceTracker::new();
        let now = now_ms();

        // Fill beyond capacity - ring buffer naturally handles this
        for i in 0..(SEQ_TRACKING_SIZE + 100) {
            seq_tracker.insert(i as u32, 1, now);
        }

        // Ring buffer should have overwritten older entries
        // Recent entries should still be accessible
        let recent_seq = (SEQ_TRACKING_SIZE + 50) as u32;
        assert!(seq_tracker.get(recent_seq, now).is_some());

        // Old entries that were overwritten should not be accessible
        // (due to collision with newer sequence numbers)
        let old_seq = 50u32;
        // The old entry was overwritten when seq (SEQ_TRACKING_SIZE + 50) was inserted
        // because they map to the same index
        assert!(seq_tracker.get(old_seq, now).is_none());
    }

    #[test]
    fn test_connection_selection_with_all_disconnected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Disconnect all connections
        for conn in &mut connections {
            conn.connected = false;
        }

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };

        let selected = select_connection_idx(&mut connections, None, 0, 0, &config);

        // Should return None when all connections have score -1
        assert_eq!(selected, None);
    }

    #[test]
    fn test_exploration_mode() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: true,
            rtt_delta_ms: 30,
        };

        // Test exploration - this is time-dependent so we just test that it doesn't panic
        let _selected = select_connection_idx(&mut connections, None, 0, 0, &config);

        // The result depends on timing, but should not panic
    }

    #[test]
    fn test_config_integration() {
        let config = DynamicConfig::new();
        let snap = config.snapshot();

        // Default values from DynamicConfig::new()
        assert_eq!(snap.mode, SchedulingMode::Enhanced);
        assert!(snap.quality_enabled);
        assert!(!snap.exploration_enabled);
        assert_eq!(snap.rtt_delta_ms, 30);
    }

    #[test]
    fn test_calculate_quality_multiplier() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt
            .block_on(create_test_connections(1))
            .into_iter()
            .next()
            .unwrap();

        let current_time = now_ms();
        conn.reconnection.connection_established_ms = current_time - 35000;

        assert_eq!(calculate_quality_multiplier(&conn, current_time), 1.1);

        // Test connection with recent NAK - exponential decay formula
        // With exponential decay: penalty = 0.5 * e^(-age_ms / 2000), multiplier = 1.0 - penalty
        conn.congestion.nak_count = 1;

        // 500ms ago: multiplier ≈ 0.61 (strong penalty, recent NAK)
        conn.congestion.last_nak_time_ms = current_time - 500;
        let mult_500 = calculate_quality_multiplier(&conn, current_time);
        assert!(
            (mult_500 - 0.61).abs() < 0.02,
            "Expected ~0.61, got {}",
            mult_500
        );

        // 2000ms ago (half-life): multiplier ≈ 0.816 (moderate penalty)
        conn.congestion.last_nak_time_ms = current_time - 2000;
        let mult_2000 = calculate_quality_multiplier(&conn, current_time);
        assert!(
            (mult_2000 - 0.816).abs() < 0.02,
            "Expected ~0.816, got {}",
            mult_2000
        );

        // 5000ms ago: multiplier ≈ 0.96 (light penalty)
        conn.congestion.last_nak_time_ms = current_time - 5000;
        let mult_5000 = calculate_quality_multiplier(&conn, current_time);
        assert!(
            (mult_5000 - 0.96).abs() < 0.02,
            "Expected ~0.96, got {}",
            mult_5000
        );

        // 15000ms ago: multiplier ≈ 1.0 (essentially recovered)
        conn.congestion.last_nak_time_ms = current_time - 15000;
        let mult_15000 = calculate_quality_multiplier(&conn, current_time);
        assert!(
            (mult_15000 - 1.0).abs() < 0.02,
            "Expected ~1.0, got {}",
            mult_15000
        );

        // Test connection with no NAKs ever - bonus
        // Need to clear the last_nak_time_ms to simulate truly no NAKs
        conn.congestion.nak_count = 0;
        conn.congestion.last_nak_time_ms = 0; // Clear NAK history
        assert_eq!(calculate_quality_multiplier(&conn, current_time), 1.1);

        // Test burst NAK penalty (requires ≥5 NAKs in burst, within 3s)
        // Burst penalty is 0.7x additional multiplier
        conn.congestion.nak_count = 5;
        conn.congestion.last_nak_time_ms = current_time - 2000;
        conn.congestion.nak_burst_count = 5;
        let mult_burst = calculate_quality_multiplier(&conn, current_time);
        // At 2000ms: base multiplier ≈ 0.816, with burst: 0.816 * 0.7 ≈ 0.571
        assert!(
            (mult_burst - 0.571).abs() < 0.02,
            "Expected ~0.571, got {}",
            mult_burst
        );
    }

    /// Build a minimal 16-byte SRT control packet carrying `srt_type` in the
    /// first two bytes (big-endian), mirroring `make_control_packet` in
    /// `srtla/tests/test_broadcast_ack.cpp`.
    fn make_srt_control(srt_type: u16) -> [u8; 16] {
        let mut pkt = [0u8; 16];
        pkt[0..2].copy_from_slice(&srt_type.to_be_bytes());
        pkt
    }

    /// Mirror the NAK-attribution arm of `process_connection_events`
    /// (`sender/packet_handler.rs`): a NAK is first routed to the uplink the
    /// `SequenceTracker` (`sender/sequence.rs`) recorded as the sender of that
    /// sequence; if that lookup misses (never tracked, or the entry expired past
    /// `SEQUENCE_TRACKING_MAX_AGE_MS`) it falls back to scanning every uplink and
    /// letting the first one still holding the sequence in its packet_log account
    /// it. Crucially, once the tracked uplink IS found the fallback is suppressed
    /// even if its `handle_nak` reports the sequence already gone — that
    /// short-circuit (`handled = true`) is the dedup behavior that stops a
    /// duplicate NAK being re-counted on a different link.
    ///
    /// We replicate exactly that decision (the real `SequenceTracker::get` and
    /// `handle_nak` from `connection/ack_nak.rs` still run) so the contract is
    /// exercised without standing up the async UDP/registration harness — the
    /// same approach `connection_tests.rs::poll_keepalive` uses for the
    /// housekeeping keepalive gate.
    ///
    /// Returns the index of the uplink that actually counted the NAK, or `None`
    /// when it was a no-op (deduped, or attributable to no uplink).
    fn attribute_nak(
        connections: &mut [SrtlaConnection],
        seq_tracker: &SequenceTracker,
        nak: u32,
        current_time_ms: u64,
    ) -> Option<usize> {
        let mut counted_by: Option<usize> = None;
        let mut handled = false;

        if let Some(conn_id) = seq_tracker.get(nak, current_time_ms)
            && let Some(pos) = connections.iter().position(|c| c.conn_id == conn_id)
        {
            if connections[pos].handle_nak(nak as i32) {
                counted_by = Some(pos);
            }
            // Found the tracked uplink: do not fall through even if the sequence
            // was already cleared (duplicate NAK), so it can't be re-counted.
            handled = true;
        }

        if !handled {
            for (i, conn) in connections.iter_mut().enumerate() {
                if conn.handle_nak(nak as i32) {
                    counted_by = Some(i);
                    break;
                }
            }
        }

        counted_by
    }

    /// Port of `srtla/tests/test_broadcast_ack.cpp` (ACK arm) plus the ACK
    /// fan-out in `process_connection_events`: an SRT ACK is broadcast to *every*
    /// uplink and is cumulative, so each link clears its own in-flight packets
    /// with seq ≤ ack; a NAK is never broadcast. We first lock the broadcast
    /// *predicate* the C test pins (an ACK classifies as ACK; a NAK / data packet
    /// does not), then drive the real cumulative `handle_srt_ack`
    /// (`connection/ack_nak.rs`) across a 3-uplink pool and assert in-flight drops
    /// only where seq ≤ ack.
    #[test]
    fn ack_reduces_in_flight() {
        // -- broadcast eligibility predicate (mirrors test_broadcast_ack.cpp) --
        let ack_pkt = make_srt_control(SRT_TYPE_ACK);
        let nak_pkt = make_srt_control(SRT_TYPE_NAK);
        let data_pkt = make_srt_control(SRT_TYPE_DATA);
        assert!(
            is_srt_ack(&ack_pkt),
            "an ACK packet must be ACK-classified (broadcast-eligible)"
        );
        assert!(!is_srt_ack(&nak_pkt), "a NAK packet is not an ACK");
        assert_eq!(
            get_packet_type(&nak_pkt),
            Some(SRT_TYPE_NAK),
            "the NAK packet must classify as NAK"
        );
        assert!(
            !is_srt_ack(&data_pkt),
            "an SRT data packet is never ACK-broadcast-eligible"
        );

        // -- cumulative ACK reduces in-flight on the correct uplink(s) --
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let now = now_ms();

        // uplink 0 sent 10/20/30 ; uplink 1 sent 15/25 ; uplink 2 sent 100 (beyond ack)
        connections[0].register_packet(10, now);
        connections[0].register_packet(20, now);
        connections[0].register_packet(30, now);
        connections[1].register_packet(15, now);
        connections[1].register_packet(25, now);
        connections[2].register_packet(100, now);
        assert_eq!(connections[0].in_flight_packets, 3);
        assert_eq!(connections[1].in_flight_packets, 2);
        assert_eq!(connections[2].in_flight_packets, 1);

        // Broadcast a cumulative ACK of 30 to every uplink, exactly as
        // process_connection_events does (`for c in connections { c.handle_srt_ack }`).
        for c in connections.iter_mut() {
            c.handle_srt_ack(30);
        }

        assert_eq!(
            connections[0].in_flight_packets, 0,
            "uplink 0: 10/20/30 all ≤ 30, cleared"
        );
        assert_eq!(
            connections[1].in_flight_packets, 0,
            "uplink 1: 15/25 ≤ 30, cleared"
        );
        assert_eq!(
            connections[2].in_flight_packets, 1,
            "uplink 2: seq 100 > 30 stays in-flight (ACK is cumulative, not blanket)"
        );
    }

    /// Port of the NAK-attribution contract (README "Implementation Details":
    /// NAKs are attributed to the uplink that originally sent the sequence,
    /// tracked via the `SequenceTracker`). Forward seq S on uplink 1, record it in
    /// the tracker, inject a NAK for S, and assert only uplink 1 is penalized
    /// (nak_count++ and in-flight−−); the other uplinks are untouched.
    #[test]
    fn nak_attributed_to_sending_uplink() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let now = now_ms();

        let seq: u32 = 500;
        // Uplink 1 is the sender: register in its packet_log + record attribution.
        connections[1].register_packet(seq as i32, now);
        let mut seq_tracker = SequenceTracker::new();
        seq_tracker.insert(seq, connections[1].conn_id, now);

        let before: Vec<i32> = connections.iter().map(|c| c.congestion.nak_count).collect();
        let before_inflight = connections[1].in_flight_packets;

        let counted = attribute_nak(&mut connections, &seq_tracker, seq, now);

        assert_eq!(
            counted,
            Some(1),
            "the NAK must be attributed to the uplink that sent the sequence"
        );
        assert_eq!(
            connections[1].congestion.nak_count,
            before[1] + 1,
            "sending uplink's nak_count increments"
        );
        assert_eq!(
            connections[1].in_flight_packets,
            before_inflight - 1,
            "sending uplink's in-flight decreases"
        );
        assert_eq!(
            connections[0].congestion.nak_count, before[0],
            "uplink 0 (did not send S) is untouched"
        );
        assert_eq!(
            connections[2].congestion.nak_count, before[2],
            "uplink 2 (did not send S) is untouched"
        );
    }

    /// Port of the documented NAK fallback: when the sequence is *not* tracked the
    /// sender scans uplinks and lets the one still holding it in its packet_log
    /// account the NAK (README: "falling back ... if unknown"). Because a sequence
    /// only ever lives in its real sender's packet_log, the fallback still lands
    /// on the originating uplink. A sequence held by NO uplink is silently ignored
    /// — never double-counted, never a panic.
    #[test]
    fn nak_unknown_uplink_fallback() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let now = now_ms();
        let seq_tracker = SequenceTracker::new(); // deliberately empty: nothing tracked

        // (a) untracked but present in uplink 2's packet_log → fallback finds it.
        let known: u32 = 700;
        connections[2].register_packet(known as i32, now);
        let before2 = connections[2].congestion.nak_count;

        let counted = attribute_nak(&mut connections, &seq_tracker, known, now);
        assert_eq!(
            counted,
            Some(2),
            "fallback attributes the NAK to the uplink still holding the sequence"
        );
        assert_eq!(connections[2].congestion.nak_count, before2 + 1);
        assert_eq!(connections[0].congestion.nak_count, 0);
        assert_eq!(connections[1].congestion.nak_count, 0);

        // (b) truly unknown: not tracked and in no packet_log → no-op.
        let counts_before: Vec<i32> = connections.iter().map(|c| c.congestion.nak_count).collect();
        let counted_unknown = attribute_nak(&mut connections, &seq_tracker, 999_999, now);
        assert_eq!(
            counted_unknown, None,
            "a sequence no uplink holds is attributable to none"
        );
        let counts_after: Vec<i32> = connections.iter().map(|c| c.congestion.nak_count).collect();
        assert_eq!(
            counts_before, counts_after,
            "an unattributable NAK must not perturb any uplink"
        );
    }

    /// Port of `srtla/tests/test_nak_dedup.cpp` (dedup within the suppression
    /// window). The Rust sender has no separate `NakDeduplicator`; dedup is
    /// structural — `handle_nak` removes the sequence from the packet_log, so a
    /// *second* NAK for the same sequence finds nothing and `handle_nak` returns
    /// false (not counted). Inside the `SequenceTracker` window both NAKs route to
    /// the same uplink, and the attribution short-circuit keeps the duplicate from
    /// falling through to another link. We assert single accounting under a
    /// *paused* virtual clock (no real sleep); the tracker's own window is driven
    /// with explicit timestamps, exactly as the C test uses explicit ms
    /// (1000, 1000+SUPPRESS_MS, …).
    #[tokio::test(start_paused = true)]
    async fn nak_dedup_within_window() {
        let mut connections = create_test_connections(2).await;
        let base = now_ms();

        let seq: u32 = 800;
        connections[0].register_packet(seq as i32, base);
        let mut seq_tracker = SequenceTracker::new();
        seq_tracker.insert(seq, connections[0].conn_id, base);
        assert_eq!(connections[0].in_flight_packets, 1);

        // First sighting inside the window: counted once on the sending uplink.
        let first = attribute_nak(&mut connections, &seq_tracker, seq, base);
        assert_eq!(first, Some(0));
        assert_eq!(connections[0].congestion.nak_count, 1, "first NAK is counted");
        assert_eq!(
            connections[0].in_flight_packets, 0,
            "first NAK clears the in-flight packet"
        );

        // Advance the paused clock well within the tracking window (no real sleep).
        advance_test_clock(Duration::from_millis(50)).await;
        let within = base + 50;
        assert!(
            50 < SEQUENCE_TRACKING_MAX_AGE_MS,
            "50ms must be inside the dedup window"
        );
        assert_eq!(
            seq_tracker.get(seq, within),
            Some(connections[0].conn_id),
            "the tracker still resolves the sequence to uplink 0 inside the window"
        );

        // Duplicate NAK inside the window: routed to the same uplink, whose
        // packet_log no longer holds the sequence → not re-counted, and the
        // short-circuit keeps the other uplink clean.
        let dup = attribute_nak(&mut connections, &seq_tracker, seq, within);
        assert_eq!(dup, None, "the duplicate NAK is a no-op (single accounting)");
        assert_eq!(
            connections[0].congestion.nak_count, 1,
            "duplicate NAK within the window is NOT double-counted"
        );
        assert_eq!(
            connections[0].in_flight_packets, 0,
            "in-flight stays cleared after the duplicate"
        );
        assert_eq!(
            connections[1].congestion.nak_count, 0,
            "the duplicate never leaks onto another uplink"
        );
    }
}
