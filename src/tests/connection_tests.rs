#[cfg(test)]
mod tests {
    use crate::protocol::*;
    use crate::test_helpers::create_test_connection;
    use crate::utils::now_ms;

    #[tokio::test(flavor = "current_thread")]
    async fn test_connection_score() {
        let mut conn = create_test_connection().await;

        // Test basic score calculation
        let initial_score = conn.get_score();
        let expected_score = WINDOW_DEF * WINDOW_MULT;
        assert_eq!(initial_score, expected_score);

        // Test with in-flight packets
        conn.in_flight_packets = 5;
        let score_with_inflight = conn.get_score();
        let expected_with_inflight = (WINDOW_DEF * WINDOW_MULT) / (5 + 1);
        assert_eq!(score_with_inflight, expected_with_inflight);

        // Test disconnected connection
        conn.connected = false;
        assert_eq!(conn.get_score(), -1);
    }

    #[test]
    fn test_packet_tracking() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Test packet registration using the public register_packet method
        let initial_in_flight = conn.in_flight_packets;

        // Register a packet directly
        conn.register_packet(100);

        assert_eq!(conn.in_flight_packets, initial_in_flight + 1);
        assert_eq!(conn.packet_log[0], 100);
        assert!(conn.packet_send_times_ms[0] > 0);

        // Test multiple packets
        for i in 1..=5 {
            conn.register_packet(100 + i);
        }
        assert_eq!(conn.in_flight_packets, initial_in_flight + 6);
    }

    #[test]
    fn test_srt_ack_handling() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Register some packets
        for i in 1..=5 {
            conn.register_packet(i * 10);
        }
        let initial_in_flight = conn.in_flight_packets;
        assert_eq!(initial_in_flight, 5);

        // ACK the first three packets (acknowledge packets 10, 20, 30)
        conn.handle_srt_ack(30);

        // Should have reduced in-flight count
        assert!(conn.in_flight_packets < 5);

        // Test window increase behavior
        let initial_window = conn.window;
        conn.consecutive_acks_without_nak = 4; // Trigger window increase
        conn.last_window_increase_ms = now_ms() - 300; // Make sure enough time passed
        conn.handle_srt_ack(40);

        assert!(conn.window >= initial_window);
    }

    #[test]
    fn test_nak_handling() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let initial_window = conn.window;

        // Register packets first (simulate sending them)
        conn.register_packet(100);
        conn.register_packet(101);
        conn.register_packet(102);
        conn.register_packet(103);

        // Test single NAK
        conn.handle_nak(100);
        assert_eq!(conn.nak_count, 1);
        assert!(conn.window < initial_window);
        assert_eq!(conn.nak_burst_count, 1);

        // Test NAK burst detection
        let current_time = now_ms();
        conn.last_nak_time_ms = current_time;

        // Simulate NAK burst (multiple NAKs within 1 second)
        conn.last_nak_time_ms = current_time;
        conn.handle_nak(101);
        assert_eq!(conn.nak_burst_count, 2);

        conn.handle_nak(102);
        assert_eq!(conn.nak_burst_count, 3);

        // Test fast recovery mode activation
        conn.window = 1500; // Low enough to trigger fast recovery
        conn.handle_nak(103);
        assert!(conn.fast_recovery_mode);
    }

    #[test]
    fn test_nak_handling_with_logged_packet() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let initial_window = conn.window;
        let initial_nak_count = conn.nak_count;

        // Register a packet (simulate sending it)
        conn.register_packet(100);

        // Now handle a NAK for that same packet
        conn.handle_nak(100);

        assert!(conn.window < initial_window, "Window should shrink on NAK");
        assert_eq!(
            conn.nak_count,
            initial_nak_count + 1,
            "NAK count should increment when packet is found"
        );
    }

    #[test]
    fn test_srtla_ack_handling() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Register some packets
        for i in 1..=3 {
            conn.register_packet(i * 100);
        }
        assert_eq!(conn.in_flight_packets, 3);

        // Test specific SRTLA ACK (using classic mode for original behavior)
        let found = conn.handle_srtla_ack_specific(200, true);
        assert!(found);
        assert_eq!(conn.in_flight_packets, 2);

        // Test not found
        let not_found = conn.handle_srtla_ack_specific(999, true);
        assert!(!not_found);
        assert_eq!(conn.in_flight_packets, 2);

        // Test global SRTLA ACK
        let initial_window = conn.window;
        conn.handle_srtla_ack_global();
        assert_eq!(conn.window, initial_window + 1);
    }

    #[test]
    fn test_classic_vs_enhanced_mode_ack_handling() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Set up connection with some packets in flight
        conn.register_packet(100);
        conn.register_packet(200);
        conn.register_packet(300);
        assert_eq!(conn.in_flight_packets, 3);

        // Set window to a moderate value
        conn.window = 1500; // Below the threshold for classic mode increase
        let initial_window = conn.window;

        // Test CLASSIC MODE: Should use simple C logic
        // With in_flight_packets=3 and window=1500, condition should be true
        // 3 * 1000 = 3000 > 1500, so SHOULD increase in classic mode
        let found = conn.handle_srtla_ack_specific(100, true); // classic_mode = true
        assert!(found);
        assert_eq!(conn.window, initial_window + WINDOW_INCR - 1); // Should increase by WINDOW_INCR - 1
        assert_eq!(conn.in_flight_packets, 2); // Should decrease

        // Reset for enhanced mode test
        conn.window = initial_window;
        conn.register_packet(400); // Add another packet
        assert_eq!(conn.in_flight_packets, 3);

        // Test ENHANCED MODE: Should use sophisticated logic
        // Reset connection state for enhanced mode test
        conn.window = 5000; // Higher window to make utilization threshold work
        conn.in_flight_packets = 0;
        conn.consecutive_acks_without_nak = 0;
        conn.last_window_increase_ms = 0; // Reset timing

        // Add packets for testing
        conn.register_packet(200);
        conn.register_packet(300);
        conn.register_packet(400);
        conn.register_packet(500);
        assert_eq!(conn.in_flight_packets, 4);

        // First ACK - should increment consecutive counter but not increase window yet
        // With window=5000, in_flight=4, utilization check should pass
        let found2 = conn.handle_srtla_ack_specific(200, false); // classic_mode = false
        assert!(found2);
        assert_eq!(conn.window, 5000); // Should NOT increase yet
        assert_eq!(conn.in_flight_packets, 3);
        assert_eq!(conn.consecutive_acks_without_nak, 1); // Should increment

        // Second ACK - should still not increase (needs 4 consecutive for normal mode)
        let found3 = conn.handle_srtla_ack_specific(300, false);
        assert!(found3);
        assert_eq!(conn.window, 5000); // Should NOT increase yet
        assert_eq!(conn.in_flight_packets, 2);
        assert_eq!(conn.consecutive_acks_without_nak, 2); // Should increment

        // Third ACK - still not enough
        let found4 = conn.handle_srtla_ack_specific(400, false);
        assert!(found4);
        assert_eq!(conn.window, 5000); // Should NOT increase yet
        assert_eq!(conn.in_flight_packets, 1);
        assert_eq!(conn.consecutive_acks_without_nak, 3); // Should increment

        // Fourth ACK - should finally increase window
        let found5 = conn.handle_srtla_ack_specific(500, false);
        assert!(found5);
        assert_eq!(conn.window, 5000 + WINDOW_INCR); // Should increase now
        assert_eq!(conn.in_flight_packets, 0);
        assert_eq!(conn.consecutive_acks_without_nak, 0); // Should reset after increase
    }

    #[test]
    fn test_keepalive_needs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Should need keepalive initially
        assert!(conn.needs_keepalive());

        // After sending, should not need immediately
        conn.last_keepalive_ms = now_ms();
        assert!(!conn.needs_keepalive());

        // After timeout, should need again
        conn.last_keepalive_ms = now_ms() - (IDLE_TIME * 1000 + 100);
        assert!(conn.needs_keepalive());
    }

    #[test]
    fn test_rtt_measurement_needs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Should need RTT measurement initially
        assert!(conn.needs_rtt_measurement());

        // After waiting for response, should not need
        conn.waiting_for_keepalive_response = true;
        assert!(!conn.needs_rtt_measurement());

        // After timeout, should need again
        conn.waiting_for_keepalive_response = false;
        conn.last_rtt_measurement_ms = now_ms() - 4000;
        assert!(conn.needs_rtt_measurement());
    }

    #[test]
    fn test_window_recovery() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Simulate some NAKs to reduce window
        for _ in 0..5 {
            conn.handle_nak(100);
        }
        let reduced_window = conn.window;

        // Simulate time passing without NAKs
        conn.last_nak_time_ms = now_ms() - 3000;
        conn.last_window_increase_ms = now_ms() - 2500;

        conn.perform_window_recovery();
        assert!(conn.window > reduced_window);
    }

    #[test]
    fn test_reconnect_logic() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Should allow first reconnect attempt
        assert!(conn.should_attempt_reconnect());

        // Record attempt
        conn.record_reconnect_attempt();
        assert_eq!(conn.reconnect_failure_count, 1);

        // Should not allow immediate retry
        assert!(!conn.should_attempt_reconnect());

        // Test backoff behavior
        let initial_time = conn.last_reconnect_attempt_ms;
        assert!(initial_time > 0);

        // Test reconnect success
        conn.mark_reconnect_success();
        assert_eq!(conn.reconnect_failure_count, 0);
    }

    #[test]
    fn test_timeout_detection() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Fresh connection should not be timed out
        assert!(!conn.is_timed_out());

        // Simulate old last_received time
        use std::time::Duration;
        conn.last_received =
            tokio::time::Instant::now().checked_sub(Duration::from_secs(CONN_TIMEOUT + 1));
        assert!(conn.is_timed_out());

        // Disconnected connection should be timed out
        conn.connected = false;
        assert!(conn.is_timed_out());
    }

    #[test]
    fn test_connection_state_management() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        assert!(conn.connected);

        // Test manual disconnection by setting connected = false
        conn.connected = false;
        assert!(!conn.connected);
        assert_eq!(conn.get_score(), -1);
    }

    #[test]
    fn test_connection_recovery_mode() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Initial state
        assert!(conn.connected);
        assert!(conn.get_score() > 0);
        let initial_window = conn.window;

        // Mark for recovery (C-style)
        conn.mark_for_recovery();

        // Connection should now be marked disconnected until registration succeeds
        assert!(!conn.connected);

        // Should be in recovery mode with reset state
        assert!(conn.is_timed_out());
        assert_eq!(conn.window, WINDOW_DEF * WINDOW_MULT);
        assert_eq!(conn.in_flight_packets, 0);

        // Score should now be -1 because the link is considered disconnected until REG3
        assert_eq!(conn.get_score(), -1);

        // Verify window was reset (note: WINDOW_DEF == initial_window by default)
        assert_eq!(conn.window, initial_window);
    }

    #[test]
    fn test_nak_statistics() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        assert_eq!(conn.total_nak_count(), 0);
        assert_eq!(conn.nak_burst_count(), 0);
        assert_eq!(conn.time_since_last_nak_ms(), None);

        conn.handle_nak(100);
        assert_eq!(conn.total_nak_count(), 1);
        assert_eq!(conn.nak_burst_count(), 1);
        assert!(conn.time_since_last_nak_ms().is_some());

        let time_since = conn.time_since_last_nak_ms().unwrap();
        assert!(time_since < 1000); // Should be very recent
    }

    #[tokio::test]
    async fn test_keepalive_creation_and_extraction() {
        let pkt = create_keepalive_packet();
        assert_eq!(pkt.len(), 10);
        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_KEEPALIVE));

        let timestamp = extract_keepalive_timestamp(&pkt).unwrap();
        assert!(timestamp > 0);

        // Timestamp should be recent (within last second)
        let now = now_ms();
        assert!((now.saturating_sub(timestamp)) < 1000);
    }

    #[test]
    fn test_fast_recovery_mode() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        assert!(!conn.fast_recovery_mode);

        // Register packet first, then reduce window to trigger fast recovery
        conn.register_packet(100);
        conn.window = 1500;
        conn.handle_nak(100);

        assert!(conn.fast_recovery_mode);

        // Test recovery exit condition
        conn.window = 15_000;
        conn.register_packet(200);
        conn.handle_srtla_ack_specific(200, false);

        assert!(!conn.fast_recovery_mode);
    }

    #[test]
    fn test_packet_log_wraparound() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Fill up the packet log beyond its size
        for i in 0..(PKT_LOG_SIZE + 10) {
            conn.register_packet(i as i32);
        }

        // Should have wrapped around
        assert_eq!(conn.packet_idx, 10);
        assert_eq!(conn.in_flight_packets, PKT_LOG_SIZE as i32 + 10);

        // Verify that old packets can still be found and acknowledged
        let recent_seq = (PKT_LOG_SIZE + 5) as i32;
        conn.handle_srt_ack(recent_seq);

        // Should have reduced in-flight count
        assert!(conn.in_flight_packets < PKT_LOG_SIZE as i32 + 10);
    }
}
