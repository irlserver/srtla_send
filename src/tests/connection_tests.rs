#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::Instant;

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
        let current_time = now_ms();

        // Test packet registration using the public register_packet method
        let initial_in_flight = conn.in_flight_packets;

        // Register a packet directly
        conn.register_packet(100, current_time);

        assert_eq!(conn.in_flight_packets, initial_in_flight + 1);
        // packet_log is now a HashMap: seq -> send_time_ms
        assert!(conn.packet_log.contains_key(&100));
        assert!(conn.packet_log.get(&100).unwrap() > &0);

        // Test multiple packets
        for i in 1..=5 {
            conn.register_packet(100 + i, current_time);
        }
        assert_eq!(conn.in_flight_packets, initial_in_flight + 6);
    }

    #[test]
    fn test_srt_ack_handling() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        // Register some packets
        for i in 1..=5 {
            conn.register_packet(i * 10, current_time);
        }
        let initial_in_flight = conn.in_flight_packets;
        assert_eq!(initial_in_flight, 5);

        // ACK the first three packets (acknowledge packets 10, 20, 30)
        conn.handle_srt_ack(30);

        // Should have reduced in-flight count
        assert!(conn.in_flight_packets < 5);

        // Test window increase behavior
        let initial_window = conn.window;
        conn.congestion.consecutive_acks_without_nak = 4; // Trigger window increase
        conn.congestion.last_window_increase_ms = now_ms() - 300; // Make sure enough time passed
        conn.handle_srt_ack(40);

        assert!(conn.window >= initial_window);
    }

    #[test]
    fn test_nak_handling() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let initial_window = conn.window;
        let current_time = now_ms();

        // Register packets first (simulate sending them)
        conn.register_packet(100, current_time);
        conn.register_packet(101, current_time);
        conn.register_packet(102, current_time);
        conn.register_packet(103, current_time);

        // Test single NAK
        conn.handle_nak(100);
        assert_eq!(conn.congestion.nak_count, 1);
        assert!(conn.window < initial_window);
        assert_eq!(conn.congestion.nak_burst_count, 0);

        // Test NAK burst detection
        let current_time = now_ms();
        conn.congestion.last_nak_time_ms = current_time;

        // Simulate NAK burst (multiple NAKs within 1 second)
        conn.congestion.last_nak_time_ms = current_time;
        conn.handle_nak(101);
        assert_eq!(conn.congestion.nak_burst_count, 2);

        conn.handle_nak(102);
        assert_eq!(conn.congestion.nak_burst_count, 3);

        // Test fast recovery mode activation
        conn.window = 1500; // Low enough to trigger fast recovery
        conn.handle_nak(103);
        assert!(conn.congestion.fast_recovery_mode);
    }

    #[test]
    fn test_nak_handling_with_logged_packet() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let initial_window = conn.window;
        let initial_nak_count = conn.congestion.nak_count;
        let current_time = now_ms();

        // Register a packet (simulate sending it)
        conn.register_packet(100, current_time);

        // Now handle a NAK for that same packet
        conn.handle_nak(100);

        assert!(conn.window < initial_window, "Window should shrink on NAK");
        assert_eq!(
            conn.congestion.nak_count,
            initial_nak_count + 1,
            "NAK count should increment when packet is found"
        );
    }

    #[test]
    fn test_nak_burst_timing() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        for i in 100..110 {
            conn.register_packet(i, current_time);
        }

        conn.handle_nak(100);
        assert_eq!(conn.congestion.nak_burst_count, 0);
        assert_eq!(conn.congestion.nak_count, 1);

        std::thread::sleep(std::time::Duration::from_millis(500));
        conn.handle_nak(101);
        assert_eq!(conn.congestion.nak_burst_count, 2);
        assert_eq!(conn.congestion.nak_count, 2);

        conn.handle_nak(102);
        assert_eq!(conn.congestion.nak_burst_count, 3);
        assert_eq!(conn.congestion.nak_count, 3);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        conn.handle_nak(103);
        assert_eq!(conn.congestion.nak_burst_count, 0);
        assert_eq!(conn.congestion.nak_count, 4);
    }

    #[test]
    fn test_nak_burst_reset_on_timeout() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        for i in 100..110 {
            conn.register_packet(i, current_time);
        }

        conn.handle_nak(100);
        conn.handle_nak(101);
        conn.handle_nak(102);
        assert_eq!(conn.congestion.nak_burst_count, 3);

        conn.perform_window_recovery();
        assert_eq!(conn.congestion.nak_burst_count, 3);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        conn.perform_window_recovery();
        assert_eq!(conn.congestion.nak_burst_count, 0);
        assert_eq!(conn.congestion.nak_burst_start_time_ms, 0);
    }

    #[test]
    fn test_nak_not_found_doesnt_affect_stats() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        conn.register_packet(100, current_time);

        let initial_nak_count = conn.congestion.nak_count;
        let initial_burst_count = conn.congestion.nak_burst_count;
        let initial_window = conn.window;

        let found = conn.handle_nak(999);
        assert!(!found);
        assert_eq!(conn.congestion.nak_count, initial_nak_count);
        assert_eq!(conn.congestion.nak_burst_count, initial_burst_count);
        assert_eq!(conn.window, initial_window);
    }

    #[test]
    fn test_nak_burst_warning_threshold() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        for i in 100..110 {
            conn.register_packet(i, current_time);
        }

        conn.handle_nak(100);
        conn.handle_nak(101);
        assert_eq!(conn.congestion.nak_burst_count, 2);

        conn.handle_nak(102);
        assert_eq!(conn.congestion.nak_burst_count, 3);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        conn.handle_nak(103);
        assert_eq!(conn.congestion.nak_burst_count, 0);
    }

    #[test]
    fn test_nak_burst_reconnect_reset() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        for i in 100..105 {
            conn.register_packet(i, current_time);
        }

        conn.handle_nak(100);
        conn.handle_nak(101);
        conn.handle_nak(102);
        assert_eq!(conn.congestion.nak_burst_count, 3);
        assert!(conn.congestion.nak_burst_start_time_ms > 0);

        rt.block_on(conn.reconnect()).unwrap();

        assert_eq!(conn.congestion.nak_burst_count, 0);
        assert_eq!(conn.congestion.nak_burst_start_time_ms, 0);
        assert_eq!(conn.congestion.nak_count, 0);
        assert_eq!(conn.congestion.last_nak_time_ms, 0);
    }

    #[test]
    fn test_srtla_ack_handling() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        // Register some packets
        for i in 1..=3 {
            conn.register_packet(i * 100, current_time);
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
        let current_time = now_ms();

        // Set up connection with some packets in flight
        conn.register_packet(100, current_time);
        conn.register_packet(200, current_time);
        conn.register_packet(300, current_time);
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
        conn.register_packet(400, current_time); // Add another packet
        assert_eq!(conn.in_flight_packets, 3);

        // Test ENHANCED MODE: Should behave identically to classic for window growth
        // The only difference is quality scoring in connection selection
        // Reset connection state for enhanced mode test
        conn.window = 5000;
        conn.in_flight_packets = 0;

        // Add packets for testing
        conn.register_packet(200, current_time);
        conn.register_packet(300, current_time);
        conn.register_packet(400, current_time);
        conn.register_packet(500, current_time);
        conn.register_packet(600, current_time);
        conn.register_packet(700, current_time);
        assert_eq!(conn.in_flight_packets, 6);

        // First ACK - should NOT increase window immediately (boundary case: 5*1000 > 5000 is false)
        // ACK decrements in_flight 6→5, then check: 5*1000 > 5000? NO (boundary)
        let found2 = conn.handle_srtla_ack_specific(200, false);
        assert!(found2);
        assert_eq!(conn.window, 5000); // Boundary case - no increase
        assert_eq!(conn.in_flight_packets, 5);

        // Add more to get above threshold
        conn.register_packet(800, current_time);
        conn.register_packet(900, current_time);
        assert_eq!(conn.in_flight_packets, 7);

        // Second ACK - decrements 7→6, check: 6*1000 > 5000 → TRUE, increase!
        let found3 = conn.handle_srtla_ack_specific(300, false);
        assert!(found3);
        assert_eq!(conn.window, 5000 + WINDOW_INCR - 1); // Should increase by WINDOW_INCR - 1
        assert_eq!(conn.in_flight_packets, 6);
    }

    #[test]
    fn test_keepalive_needs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Should need keepalive initially
        assert!(conn.needs_keepalive());

        // After sending, should not need immediately
        conn.last_sent = Some(Instant::now());
        assert!(!conn.needs_keepalive());

        // After timeout, should need again (simulate 2 seconds ago)
        conn.last_sent = Some(Instant::now() - Duration::from_secs(IDLE_TIME + 1));
        assert!(conn.needs_keepalive());
    }

    #[test]
    fn test_rtt_measurement_needs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Should need RTT measurement initially
        assert!(conn.needs_rtt_measurement());

        // After waiting for response, should not need
        conn.rtt.waiting_for_keepalive_response = true;
        assert!(!conn.needs_rtt_measurement());

        // After timeout, should need again
        conn.rtt.waiting_for_keepalive_response = false;
        conn.rtt.last_rtt_measurement_ms = now_ms() - 4000;
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
        conn.congestion.last_nak_time_ms = now_ms() - 3000;
        conn.congestion.last_window_increase_ms = now_ms() - 2500;

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
        assert_eq!(conn.reconnection.reconnect_failure_count, 1);

        // Should not allow immediate retry
        assert!(!conn.should_attempt_reconnect());

        // Test backoff behavior
        let initial_time = conn.reconnection.last_reconnect_attempt_ms;
        assert!(initial_time > 0);

        // Test reconnect success
        conn.mark_reconnect_success();
        assert_eq!(conn.reconnection.reconnect_failure_count, 0);
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
        let current_time = now_ms();

        assert_eq!(conn.congestion.nak_count, 0);
        assert_eq!(conn.congestion.nak_burst_count, 0);
        assert_eq!(conn.time_since_last_nak_ms(), None);

        conn.register_packet(100, current_time);
        conn.handle_nak(100);
        assert_eq!(conn.congestion.nak_count, 1);
        assert_eq!(conn.congestion.nak_burst_count, 0);
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
        let current_time = now_ms();

        assert!(!conn.congestion.fast_recovery_mode);

        // Register packet first, then reduce window to trigger fast recovery
        conn.register_packet(100, current_time);
        conn.window = 1500;
        conn.handle_nak(100);

        assert!(conn.congestion.fast_recovery_mode);

        // Test recovery exit condition
        conn.window = 15_000;
        conn.register_packet(200, current_time);
        conn.handle_srtla_ack_specific(200, false);

        assert!(!conn.congestion.fast_recovery_mode);
    }

    #[test]
    fn test_packet_log_capacity() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        // Add more packets than default capacity - HashMap grows dynamically
        for i in 0..(PKT_LOG_SIZE + 10) {
            conn.register_packet(i as i32, current_time);
        }

        // HashMap stores all packets (no wraparound like fixed array)
        assert_eq!(conn.packet_log.len(), PKT_LOG_SIZE + 10);
        assert_eq!(conn.in_flight_packets, PKT_LOG_SIZE as i32 + 10);

        // Verify that packets can be found and acknowledged
        let recent_seq = (PKT_LOG_SIZE + 5) as i32;
        conn.handle_srt_ack(recent_seq);

        // Should have reduced in-flight count and removed acked packets from log
        assert!(conn.in_flight_packets < PKT_LOG_SIZE as i32 + 10);
    }

    #[test]
    fn test_progressive_window_recovery_rates() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        // Reduce window through NAKs
        conn.register_packet(100, current_time);
        conn.handle_nak(100);
        let reduced_window = conn.window;

        // Test 1: Recent NAKs (<5 seconds) - should recover at 25% rate (minimal)
        conn.congestion.last_nak_time_ms = now_ms() - 3000; // 3 seconds ago
        conn.congestion.last_window_increase_ms = now_ms() - 2500; // Allow recovery
        let before_recovery = conn.window;
        conn.perform_window_recovery();
        let recovery_amount_25 = conn.window - before_recovery;
        // Should be WINDOW_INCR * 1 / 4 = 30 / 4 = 7 (rounded down)
        assert_eq!(
            recovery_amount_25,
            WINDOW_INCR / 4,
            "Recent NAKs should recover at 25% rate"
        );

        // Test 2: 5-7 seconds since last NAK - should recover at 50% rate (slow)
        conn.window = reduced_window;
        conn.congestion.last_nak_time_ms = now_ms() - 6000; // 6 seconds ago
        conn.congestion.last_window_increase_ms = now_ms() - 2500; // Allow recovery
        let before_recovery = conn.window;
        conn.perform_window_recovery();
        let recovery_amount_50 = conn.window - before_recovery;
        // Should be WINDOW_INCR * 1 / 2 = 30 / 2 = 15
        assert_eq!(
            recovery_amount_50,
            WINDOW_INCR / 2,
            "5-7 seconds should recover at 50% rate"
        );

        // Test 3: 7-10 seconds since last NAK - should recover at 100% rate (moderate)
        conn.window = reduced_window;
        conn.congestion.last_nak_time_ms = now_ms() - 8000; // 8 seconds ago
        conn.congestion.last_window_increase_ms = now_ms() - 2500; // Allow recovery
        let before_recovery = conn.window;
        conn.perform_window_recovery();
        let recovery_amount_100 = conn.window - before_recovery;
        // Should be WINDOW_INCR * 1 = 30
        assert_eq!(
            recovery_amount_100, WINDOW_INCR,
            "7-10 seconds should recover at 100% rate"
        );

        // Test 4: 10+ seconds since last NAK - should recover at 200% rate (aggressive)
        conn.window = reduced_window;
        conn.congestion.last_nak_time_ms = now_ms() - 11000; // 11 seconds ago
        conn.congestion.last_window_increase_ms = now_ms() - 2500; // Allow recovery
        let before_recovery = conn.window;
        conn.perform_window_recovery();
        let recovery_amount_200 = conn.window - before_recovery;
        // Should be WINDOW_INCR * 2 = 30 * 2 = 60
        assert_eq!(
            recovery_amount_200,
            WINDOW_INCR * 2,
            "10+ seconds should recover at 200% rate"
        );

        // Verify progressive rates: 25% < 50% < 100% < 200%
        assert!(recovery_amount_25 < recovery_amount_50);
        assert!(recovery_amount_50 < recovery_amount_100);
        assert!(recovery_amount_100 < recovery_amount_200);
    }

    #[test]
    fn test_progressive_recovery_with_fast_mode() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        // Reduce window and trigger fast recovery mode
        conn.register_packet(100, current_time);
        conn.window = 1500;
        conn.handle_nak(100);
        assert!(conn.congestion.fast_recovery_mode);

        let reduced_window = conn.window;

        // Test fast mode with recent NAKs - should be 25% * 2 = 50% of normal rate
        conn.congestion.last_nak_time_ms = now_ms() - 3000; // 3 seconds ago
        conn.congestion.last_window_increase_ms = now_ms() - 600; // Allow fast recovery timing
        let before_recovery = conn.window;
        conn.perform_window_recovery();
        let fast_recovery_25 = conn.window - before_recovery;
        // Should be WINDOW_INCR * 2 / 4 = 30 * 2 / 4 = 15
        assert_eq!(
            fast_recovery_25,
            (WINDOW_INCR * 2) / 4,
            "Fast mode recent NAKs should be 50% of normal rate"
        );

        // Test fast mode with 10+ seconds - should be 200% * 2 = 400% of normal rate
        conn.window = reduced_window;
        conn.congestion.last_nak_time_ms = now_ms() - 11000; // 11 seconds ago
        conn.congestion.last_window_increase_ms = now_ms() - 600; // Allow fast recovery timing
        let before_recovery = conn.window;
        conn.perform_window_recovery();
        let fast_recovery_200 = conn.window - before_recovery;
        // Should be WINDOW_INCR * 2 * 2 = 30 * 2 * 2 = 120
        assert_eq!(
            fast_recovery_200,
            WINDOW_INCR * 2 * 2,
            "Fast mode 10+ seconds should be 400% of normal rate"
        );

        // Fast recovery should be significantly faster than normal
        assert!(fast_recovery_200 > WINDOW_INCR * 2);
    }

    #[test]
    fn test_progressive_recovery_timing_constraints() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let current_time = now_ms();

        // Reduce window
        conn.register_packet(100, current_time);
        conn.handle_nak(100);
        let reduced_window = conn.window;

        // Test normal mode timing constraint (2000ms min wait + 1000ms increment wait)
        conn.congestion.last_nak_time_ms = now_ms() - 8000; // 8 seconds ago (should trigger)
        conn.congestion.last_window_increase_ms = now_ms() - 500; // Too recent
        let before = conn.window;
        conn.perform_window_recovery();
        // Should NOT recover because increment wait time not met
        assert_eq!(
            conn.window, before,
            "Should not recover when timing constraint not met"
        );

        // Now allow enough time
        conn.congestion.last_window_increase_ms = now_ms() - 1500; // Enough time
        conn.perform_window_recovery();
        // Should recover now
        assert!(
            conn.window > before,
            "Should recover when timing constraint is met"
        );

        // Test fast mode timing constraint (500ms min wait + 300ms increment wait)
        conn.window = reduced_window;
        conn.congestion.fast_recovery_mode = true;
        conn.congestion.last_nak_time_ms = now_ms() - 8000; // 8 seconds ago
        conn.congestion.last_window_increase_ms = now_ms() - 200; // Too recent even for fast mode
        let before_fast = conn.window;
        conn.perform_window_recovery();
        // Should NOT recover
        assert_eq!(
            conn.window, before_fast,
            "Fast mode should not recover when timing constraint not met"
        );

        // Now allow enough time for fast mode
        conn.congestion.last_window_increase_ms = now_ms() - 400; // Enough for fast mode
        conn.perform_window_recovery();
        // Should recover now
        assert!(
            conn.window > before_fast,
            "Fast mode should recover when timing constraint is met"
        );
    }
}
