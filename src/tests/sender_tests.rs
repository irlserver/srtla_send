#[cfg(test)]
mod tests {
    #![allow(clippy::assertions_on_constants)]

    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr};

    use smallvec::SmallVec;
    use tempfile::NamedTempFile;

    use crate::config::{ConfigSnapshot, DynamicConfig};
    use crate::mode::SchedulingMode;
    use crate::sender::*;
    use crate::test_helpers::create_test_connections;
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
            ..ConfigSnapshot::default()
        };

        let selected = select_connection_idx(&mut connections, None, 0, &config, true);
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn test_enhanced_skips_weak_when_alternative_exists() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let current_time = now_ms();

        // Connection 1 has the highest base score but is flagged weak.
        // Connection 0 is healthy. Selection should pick 0, not 1.
        connections[0].in_flight_packets = 5;
        connections[1].in_flight_packets = 0;
        connections[1].weak = true;
        connections[2].in_flight_packets = 10;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, current_time, &config, true);
        assert_eq!(
            selected,
            Some(0),
            "weak connection 1 must be skipped when a non-weak alternative exists"
        );
    }

    #[test]
    fn test_enhanced_falls_back_when_all_weak() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let current_time = now_ms();

        // Every link is weak. Selection must still pick the best — better
        // a weak link than a dropped packet.
        connections[0].weak = true;
        connections[0].in_flight_packets = 5;
        connections[1].weak = true;
        connections[1].in_flight_packets = 0; // best score among the weak
        connections[2].weak = true;
        connections[2].in_flight_packets = 10;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, current_time, &config, true);
        assert_eq!(
            selected,
            Some(1),
            "with no non-weak alternatives, selection must fall back to the best available link"
        );
    }

    #[test]
    fn test_enhanced_skips_in_flight_cap_when_alternative_exists() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let current_time = now_ms();

        // Connection 1 would have the best base score (lowest in_flight)
        // but is over its BDP in-flight cap: cc_target_bps = 200 kbps at
        // the test RTT (~200 ms) gives a cap of ~5 packets, and
        // in_flight = 6 exceeds it. Connection 0 is unconstrained, so the
        // capped link must be skipped even though its score is higher.
        connections[0].in_flight_packets = 12;
        connections[1].in_flight_packets = 6;
        connections[1].cc_target_bps = 200_000;
        connections[2].in_flight_packets = 20;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, current_time, &config, true);
        assert_eq!(
            selected,
            Some(0),
            "in-flight-capped link must be skipped when an un-gated alternative exists"
        );
    }

    #[test]
    fn test_enhanced_falls_back_when_all_in_flight_capped() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let current_time = now_ms();

        // Every link is over its BDP in-flight cap (cc_target = 200 kbps
        // at ~200 ms RTT → cap ~5 packets). Fallback rule: pick the best
        // base score rather than drop the packet.
        for c in connections.iter_mut() {
            c.cc_target_bps = 200_000;
            c.in_flight_packets = 10;
        }
        connections[1].in_flight_packets = 6; // best score among the capped, still > cap

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, current_time, &config, true);
        assert_eq!(
            selected,
            Some(1),
            "with no un-gated alternatives, selection falls back to the best capped link"
        );
    }

    #[test]
    fn test_enhanced_treats_loss_degraded_as_weak() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let current_time = now_ms();

        connections[0].in_flight_packets = 5;
        connections[1].in_flight_packets = 0;
        // Sustained loss latch (not the raw per-window cc_backing_off) is the
        // routing-admission gate, so a single noisy loss window can't demote.
        connections[1].loss_degraded = true;
        connections[2].in_flight_packets = 10;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, current_time, &config, true);
        assert_eq!(
            selected,
            Some(0),
            "loss-degraded link must be skipped when a healthy alternative exists"
        );
    }

    #[test]
    fn test_enhanced_does_not_gate_on_raw_backing_off() {
        // cc_backing_off drives the CC controller's bitrate backoff but is
        // intentionally NOT a routing gate (it flips on a single loss window).
        // A link flagged only cc_backing_off, with the best base score, still
        // wins selection.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let current_time = now_ms();

        connections[0].in_flight_packets = 5;
        connections[1].in_flight_packets = 0; // best base score
        connections[1].cc_backing_off = true;
        connections[2].in_flight_packets = 10;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, current_time, &config, true);
        assert_eq!(
            selected,
            Some(1),
            "cc_backing_off alone must not demote a link's routing weight"
        );
    }

    #[test]
    fn test_enhanced_weak_link_stays_reachable_for_exploration() {
        // A2: a quality-gated link is crushed in score but not removed,
        // so exploration can still probe it. Without that, the gated link
        // is never ranked second-best, exploration can't reach it, it
        // earns zero throughput share, and the classifier keeps it weak
        // forever (starvation lock).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));
        let current_time = now_ms();

        // Connection 0 is the current best but has a recent NAK
        // (degrading). Connection 1 is weak but has no NAKs (recovered).
        // Exploration's degraded-best + recovered-second path fires
        // deterministically, independent of wall-clock.
        connections[0].in_flight_packets = 0;
        connections[0].congestion.nak_count = 1;
        connections[0].congestion.last_nak_time_ms = current_time.saturating_sub(1000);
        connections[1].in_flight_packets = 0;
        connections[1].weak = true;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: true,
            ..ConfigSnapshot::default()
        };
        // last_idx = 0 (current best), well outside the switch cooldown.
        let selected = select_connection_idx(&mut connections, Some(0), current_time, &config, true);
        assert_eq!(
            selected,
            Some(1),
            "weak link must remain rankable so exploration can probe it"
        );
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
            ..ConfigSnapshot::default()
        };

        let selected = select_connection_idx(&mut connections, None, current_time, &config, true);

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
            ..ConfigSnapshot::default()
        };

        let selected = select_connection_idx(&mut connections, None, current_time, &config, true);

        // Should prefer connection 2 (never had NAKs, best quality)
        assert_eq!(selected, Some(2));
    }

    #[test]
    fn test_enhanced_reselects_immediately_no_time_lock() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup: Connection 0 is currently selected, Connection 1 has better score
        connections[0].in_flight_packets = 5; // Lower score
        connections[1].in_flight_packets = 0; // Best score
        connections[2].in_flight_packets = 10; // Worst score

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };

        // There is no switch cooldown: selection must re-decide on every packet.
        // `get_score()` counts queued packets as in-flight, so routing a packet
        // lowers its own link's score -- that feedback loop is what bounds
        // per-link queue depth, and a time lock would open it.
        let selected = select_connection_idx(&mut connections, Some(0), now_ms(), &config, true);
        assert_eq!(
            selected,
            Some(1),
            "Enhanced mode must be free to switch to a better link on the very next packet"
        );
    }

    #[test]
    fn test_enhanced_switches_to_clearly_better_connection() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup: Connection 0 is currently selected, Connection 1 has significantly better score
        connections[0].in_flight_packets = 5; // Lower score
        connections[1].in_flight_packets = 0; // Best score (significantly better, exceeds 2% hysteresis)
        connections[2].in_flight_packets = 10; // Worst score

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };

        // A link better by more than SWITCH_THRESHOLD wins the packet.
        let selected = select_connection_idx(&mut connections, Some(0), now_ms(), &config, true);
        assert_eq!(
            selected,
            Some(1),
            "Should route to the better connection"
        );
    }

    #[test]
    fn test_enhanced_switches_away_from_timed_out_connection() {
        use tokio::time::{Duration, Instant};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup: Connection 0 is currently selected but becomes timed out
        connections[0].in_flight_packets = 5;
        // Simulate timeout by setting last_received to 6 seconds ago (CONN_TIMEOUT is 5 seconds)
        connections[0].last_received = Some(Instant::now() - Duration::from_secs(6));
        connections[1].in_flight_packets = 0; // Best score
        connections[2].in_flight_packets = 10;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };

        let selected = select_connection_idx(&mut connections, Some(0), now_ms(), &config, true);
        assert_eq!(
            selected,
            Some(1),
            "Should route via a valid connection when the current one has timed out"
        );
    }

    #[test]
    fn test_classic_mode_picks_highest_score() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // Setup: Connection 0 is currently selected, Connection 1 has better score
        connections[0].in_flight_packets = 5; // Lower score
        connections[1].in_flight_packets = 0; // Best score
        connections[2].in_flight_packets = 10; // Worst score

        let config = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };

        // Classic mode: per-packet selection ALWAYS picks highest score connection
        // No hysteresis - matches original C implementation
        let selected = select_connection_idx(&mut connections, Some(0), now_ms(), &config, true);

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

        let binder: std::sync::Arc<dyn crate::connection::UplinkBinder> =
            std::sync::Arc::new(crate::connection::SourceIpBinder);
        rt.block_on(apply_connection_changes(
            &mut connections,
            &new_ips,
            "127.0.0.1",
            8080,
            &mut last_selected_idx,
            &mut seq_tracker,
            &binder,
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
        let binder: std::sync::Arc<dyn crate::connection::UplinkBinder> =
            std::sync::Arc::new(crate::connection::SourceIpBinder);
        let connections = create_connections_from_ips(&ips, "127.0.0.1", 9999, &binder).await;

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
            ..ConfigSnapshot::default()
        };

        let selected = select_connection_idx(&mut connections, None, 0, &config, true);

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
            ..ConfigSnapshot::default()
        };

        // Test exploration - this is time-dependent so we just test that it doesn't panic
        let _selected = select_connection_idx(&mut connections, None, 0, &config, true);

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

    // ---- link phase as a weight, not a gate --------------------------------

    #[test]
    fn test_warming_link_is_schedulable() {
        // At go-live EVERY link is warming. When Warming was a hard exclusion the
        // candidate pool was empty and the sender dropped the stream until the
        // first link was promoted. A warming link must be usable.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));
        let now = now_ms();

        for c in connections.iter_mut() {
            c.phase = crate::connection::LinkPhase::Warming {
                rtt_probes: 0,
                entered_ms: now,
            };
        }
        connections[0].in_flight_packets = 5;
        connections[1].in_flight_packets = 0; // best

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, now, &config, true);
        assert_eq!(
            selected,
            Some(1),
            "an all-warming pool must still schedule, not drop the packet"
        );
    }

    #[test]
    fn test_warming_link_is_derated_against_a_live_one() {
        // The de-rating is what Warming buys us: a link whose RTT baseline is a
        // keepalive old should not take a full share while a characterised link
        // is available. 0.8 x a marginally better raw score loses to Live.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));
        let now = now_ms();

        // Warming link has the better *raw* score (fewer in flight)...
        connections[0].phase = crate::connection::LinkPhase::Warming {
            rtt_probes: 1,
            entered_ms: now,
        };
        connections[0].in_flight_packets = 4;
        // ...but the Live link is close enough that the 0.8 weight flips it.
        connections[1].in_flight_packets = 5;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, now, &config, true);
        assert_eq!(
            selected,
            Some(1),
            "a warming link's 0.8 weight should cede a close call to a live link"
        );
    }

    #[test]
    fn test_registering_link_is_never_scheduled() {
        // The one hard exclusion, and it is not a quality judgement: without REG3
        // the receiver discards data on this link, so sending is pointless.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));
        let now = now_ms();

        connections[0].phase = crate::connection::LinkPhase::Registering;
        connections[0].in_flight_packets = 0; // would otherwise be the best score
        connections[1].in_flight_packets = 10;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, None, now, &config, true);
        assert_eq!(
            selected,
            Some(1),
            "a link that has not completed REG3 must never be scheduled"
        );
    }

    // ---- starved-link probing (exploration) --------------------------------
    //
    // The probe exists to break the starvation lock: a gated link wins no
    // packets, so it earns no ACKs, so the signal that gated it never clears.
    // These tests pin the properties that keep it from being harmful.

    fn exploring() -> ConfigSnapshot {
        ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: true,
            ..ConfigSnapshot::default()
        }
    }

    #[test]
    fn test_probe_targets_the_starved_link_not_second_best() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let now = now_ms();

        // 0 is the healthy best. 2 is healthy but merely second-best -- it is
        // already earning ACKs and needs no probe. 1 is starved.
        connections[0].in_flight_packets = 0;
        connections[2].in_flight_packets = 5;
        connections[1].in_flight_packets = 1;
        connections[1].weak = true;

        let selected = select_connection_idx(&mut connections, Some(0), now, &exploring(), true);
        assert_eq!(
            selected,
            Some(1),
            "probe must go to the starved link, not the healthy second-best"
        );
    }

    #[test]
    fn test_probe_is_rate_limited_per_link() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let now = now_ms();

        connections[0].in_flight_packets = 0;
        connections[2].in_flight_packets = 5;
        connections[1].in_flight_packets = 1;
        connections[1].weak = true;

        // First packet probes the starved link and stamps it.
        let first = select_connection_idx(&mut connections, Some(0), now, &exploring(), true);
        assert_eq!(first, Some(1));

        // The very next packet must go back to the healthy link: a probe is one
        // packet, not a mode. Without the budget this alternates every packet.
        let second = select_connection_idx(&mut connections, Some(1), now + 1, &exploring(), true);
        assert_eq!(
            second,
            Some(0),
            "a second probe must not fire inside PROBE_INTERVAL_MS"
        );

        // Still suppressed just before the interval elapses.
        let during = select_connection_idx(
            &mut connections,
            Some(0),
            now + PROBE_INTERVAL_MS - 1,
            &exploring(),
            true,
        );
        assert_eq!(during, Some(0), "probe budget must hold for the full interval");

        // Due again once the interval has passed.
        let after = select_connection_idx(
            &mut connections,
            Some(0),
            now + PROBE_INTERVAL_MS,
            &exploring(),
            true,
        );
        assert_eq!(after, Some(1), "probe should be due again after the interval");
    }

    #[test]
    fn test_probe_never_fires_without_a_healthy_link() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let now = now_ms();

        // Everything is starved: there is no spare capacity to fund a probe,
        // and normal scoring already routes to the least-bad link. Diverting
        // here would just add latency to a packet the stream needs.
        for c in connections.iter_mut() {
            c.weak = true;
            c.in_flight_packets = 5;
        }
        connections[1].in_flight_packets = 0; // best of a bad lot

        let selected = select_connection_idx(&mut connections, Some(1), now, &exploring(), true);
        assert_eq!(
            selected,
            Some(1),
            "with no healthy link, selection must fall back to the best link and not probe"
        );
    }

    #[test]
    fn test_probe_only_diverts_data_packets() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let now = now_ms();

        connections[0].in_flight_packets = 0;
        connections[2].in_flight_packets = 5;
        connections[1].in_flight_packets = 1;
        connections[1].weak = true;

        // A control packet carries no sequence number, so it can never earn the
        // ACK/NAK a probe is collecting -- and steering SRT's own control
        // traffic onto a degraded link would delay its control loop for nothing.
        let selected = select_connection_idx(&mut connections, Some(0), now, &exploring(), false);
        assert_eq!(
            selected,
            Some(0),
            "control packets must never be diverted to a starved link"
        );
    }

    #[test]
    fn test_no_probe_when_exploration_disabled() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));
        let now = now_ms();

        connections[0].in_flight_packets = 0;
        connections[2].in_flight_packets = 5;
        connections[1].in_flight_packets = 1;
        connections[1].weak = true;

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut connections, Some(0), now, &config, true);
        assert_eq!(
            selected,
            Some(0),
            "probing must stay off unless exploration is enabled"
        );
    }
}
