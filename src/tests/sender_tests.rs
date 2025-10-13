#[cfg(test)]
mod tests {

    use std::collections::{HashMap, VecDeque};
    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::Ordering;

    use smallvec::SmallVec;
    use tempfile::NamedTempFile;
    use tokio::time::Instant;

    use crate::sender::*;
    use crate::test_helpers::create_test_connections;
    use crate::toggles::DynamicToggles;
    use crate::utils::now_ms;

    #[test]
    fn test_select_connection_idx_classic() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Test classic mode - should pick connection with highest score
        connections[1].in_flight_packets = 0; // Best score
        connections[0].in_flight_packets = 5; // Lower score
        connections[2].in_flight_packets = 10; // Lowest score

        let selected =
            select_connection_idx(&connections, None, false, false, true, Instant::now());
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn test_select_connection_idx_quality_scoring() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Connection 0: Recent NAKs - should get low score
        connections[0].nak_count = 5;
        connections[0].last_nak_time_ms = now_ms() - 1000; // 1 second ago

        // Connection 1: No NAKs - should get bonus
        connections[1].nak_count = 0;

        // Connection 2: Old NAKs - should get partial penalty
        connections[2].nak_count = 3;
        connections[2].last_nak_time_ms = now_ms() - 8000; // 8 seconds ago

        let selected =
            select_connection_idx(&connections, None, true, false, false, Instant::now());

        // Should prefer connection 1 (no NAKs)
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn test_select_connection_idx_burst_nak_penalty() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Connection 0: NAK burst
        connections[0].nak_count = 5;
        connections[0].nak_burst_count = 3;
        connections[0].last_nak_time_ms = now_ms() - 2000; // 2 seconds ago

        // Connection 1: Same NAK count but no burst
        connections[1].nak_count = 5;
        connections[1].nak_burst_count = 0;
        connections[1].last_nak_time_ms = now_ms() - 2000; // 2 seconds ago

        let selected =
            select_connection_idx(&connections, None, true, false, false, Instant::now());

        // Should prefer connection 2 (never had NAKs, best quality)
        assert_eq!(selected, Some(2));
    }

    #[test]
    fn test_nak_attribution_to_correct_connection() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        connections[0].register_packet(100);
        connections[1].register_packet(200);
        connections[2].register_packet(300);

        let initial_counts = [
            connections[0].nak_count,
            connections[1].nak_count,
            connections[2].nak_count,
        ];

        let found_0 = connections[0].handle_nak(100);
        assert!(found_0);
        assert_eq!(connections[0].nak_count, initial_counts[0] + 1);
        assert_eq!(connections[1].nak_count, initial_counts[1]);
        assert_eq!(connections[2].nak_count, initial_counts[2]);

        let found_1 = connections[1].handle_nak(200);
        assert!(found_1);
        assert_eq!(connections[0].nak_count, initial_counts[0] + 1);
        assert_eq!(connections[1].nak_count, initial_counts[1] + 1);
        assert_eq!(connections[2].nak_count, initial_counts[2]);

        let not_found_0 = connections[0].handle_nak(999);
        let not_found_1 = connections[1].handle_nak(999);
        let not_found_2 = connections[2].handle_nak(999);
        assert!(!not_found_0);
        assert!(!not_found_1);
        assert!(!not_found_2);
        assert_eq!(connections[0].nak_count, initial_counts[0] + 1);
        assert_eq!(connections[1].nak_count, initial_counts[1] + 1);
        assert_eq!(connections[2].nak_count, initial_counts[2]);
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
        let mut seq_to_conn = HashMap::new();
        let now = now_ms();
        seq_to_conn.insert(
            100,
            SequenceTrackingEntry {
                conn_id: connections[1].conn_id,
                timestamp_ms: now,
            },
        );
        seq_to_conn.insert(
            200,
            SequenceTrackingEntry {
                conn_id: connections[2].conn_id,
                timestamp_ms: now,
            },
        );
        let mut seq_order = VecDeque::new();
        seq_order.push_back(100);
        seq_order.push_back(200);

        rt.block_on(apply_connection_changes(
            &mut connections,
            &new_ips,
            "127.0.0.1",
            8080,
            &mut last_selected_idx,
            &mut seq_to_conn,
            &mut seq_order,
        ));

        // Should have removed some connections
        assert!(connections.len() < initial_count);

        // Should have reset selection
        assert_eq!(last_selected_idx, None);

        // Should have cleaned up sequence tracking
        assert!(seq_to_conn.len() < 2);

        // Should have cleaned up seq_order to match seq_to_conn
        assert_eq!(seq_order.len(), seq_to_conn.len());
    }

    #[test]
    fn test_pending_connection_changes() {
        let changes = PendingConnectionChanges {
            new_ips: Some(SmallVec::from_vec(vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))])),
            receiver_host: "test-host".to_string(),
            receiver_port: 9090,
        };

        assert!(changes.new_ips.is_some());
        assert_eq!(changes.receiver_host, "test-host");
        assert_eq!(changes.receiver_port, 9090);
    }

    #[test]
    fn test_constants() {
        assert!(MAX_SEQUENCE_TRACKING > 0);
        assert!(GLOBAL_TIMEOUT_MS > 0);

        // Should handle decent throughput
        assert!(MAX_SEQUENCE_TRACKING >= 1000);
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
        let mut seq_to_conn: HashMap<u32, usize> = HashMap::with_capacity(MAX_SEQUENCE_TRACKING);
        let mut seq_order: std::collections::VecDeque<u32> =
            std::collections::VecDeque::with_capacity(MAX_SEQUENCE_TRACKING);

        // Fill beyond capacity
        for i in 0..(MAX_SEQUENCE_TRACKING + 100) {
            if seq_to_conn.len() >= MAX_SEQUENCE_TRACKING
                && let Some(old) = seq_order.pop_front()
            {
                seq_to_conn.remove(&old);
            }
            seq_to_conn.insert(i as u32, 0);
            seq_order.push_back(i as u32);
        }

        // Should not exceed maximum
        assert!(seq_to_conn.len() <= MAX_SEQUENCE_TRACKING);
        assert!(seq_order.len() <= MAX_SEQUENCE_TRACKING + 100); // VecDeque grows but HashMap doesn't
    }

    #[test]
    fn test_connection_selection_with_all_disconnected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Disconnect all connections
        for conn in &mut connections {
            conn.connected = false;
        }

        let selected =
            select_connection_idx(&connections, None, false, false, false, Instant::now());

        // Should return None when all connections have score -1
        assert_eq!(selected, None);
    }

    #[test]
    fn test_exploration_mode() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let connections = rt.block_on(create_test_connections(3));

        // Test exploration - this is time-dependent so we just test that it doesn't panic
        let _selected =
            select_connection_idx(&connections, None, false, true, false, Instant::now());

        // The result depends on timing, but should not panic
    }

    #[test]
    fn test_dynamic_toggles_integration() {
        let toggles = DynamicToggles::new();

        // Test that toggles can be read atomically
        let classic = toggles.classic_mode.load(Ordering::Relaxed);
        let quality = toggles.quality_scoring_enabled.load(Ordering::Relaxed);
        let explore = toggles.exploration_enabled.load(Ordering::Relaxed);

        // Default values from DynamicToggles::new()
        assert!(!classic);
        assert!(quality);
        assert!(!explore);
    }

    #[test]
    fn test_calculate_quality_multiplier() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt
            .block_on(create_test_connections(1))
            .into_iter()
            .next()
            .unwrap();

        conn.connection_established_ms = now_ms() - 15000;

        assert_eq!(calculate_quality_multiplier(&conn), 1.2);

        // Test connection with recent NAK (< 2 seconds ago) - heavy penalty
        conn.nak_count = 1;
        conn.last_nak_time_ms = now_ms() - 1000;
        assert_eq!(calculate_quality_multiplier(&conn), 0.1);

        // Test connection with NAK 3-5 seconds ago - moderate penalty
        conn.last_nak_time_ms = now_ms() - 3000;
        assert_eq!(calculate_quality_multiplier(&conn), 0.5);

        // Test connection with NAK 5-10 seconds ago - light penalty
        conn.last_nak_time_ms = now_ms() - 7000;
        assert_eq!(calculate_quality_multiplier(&conn), 0.8);

        // Test connection with NAK > 10 seconds ago and no total NAKs - bonus
        conn.last_nak_time_ms = now_ms() - 15000;
        conn.nak_count = 0;
        assert_eq!(calculate_quality_multiplier(&conn), 1.2);

        // Test connection with NAK > 10 seconds ago and some NAKs - no penalty/bonus
        conn.nak_count = 5;
        assert_eq!(calculate_quality_multiplier(&conn), 1.0);

        // Test burst NAK penalty
        conn.last_nak_time_ms = now_ms() - 3000;
        conn.nak_burst_count = 3;
        assert_eq!(calculate_quality_multiplier(&conn), 0.5 * 0.5); // 0.5 * 0.5 = 0.25
    }
}
