#[cfg(test)]
mod tests {
    use std::time::Duration;


    use crate::protocol::*;
    use crate::sender::{SEQUENCE_TRACKING_MAX_AGE_MS, SequenceTracker, attribute_nak};
    use crate::test_helpers::{
        advance_test_clock, create_test_connection, create_test_connections,
    };
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

        conn.perform_window_recovery(now_ms());
        assert_eq!(conn.congestion.nak_burst_count, 3);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        conn.perform_window_recovery(now_ms());
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

        let now = now_ms();

        // Should need keepalive initially (last_keepalive_sent is None)
        assert!(conn.needs_keepalive(now));

        // After sending keepalive, should not need immediately
        conn.last_keepalive_sent = Some(now);
        assert!(!conn.needs_keepalive(now));

        // After timeout, should need again (stamp IDLE_TIME + 1 seconds in the past)
        conn.last_keepalive_sent = Some(now - (IDLE_TIME + 1) * 1000);
        assert!(conn.needs_keepalive(now));
    }

    #[test]
    fn test_rtt_measurement_needs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Should need RTT measurement initially
        assert!(conn.needs_rtt_measurement(now_ms()));

        // After waiting for response, should not need
        conn.rtt.waiting_for_keepalive_response = true;
        assert!(!conn.needs_rtt_measurement(now_ms()));

        // After timeout, should need again
        conn.rtt.waiting_for_keepalive_response = false;
        conn.rtt.last_rtt_measurement_ms = now_ms() - 4000;
        assert!(conn.needs_rtt_measurement(now_ms()));
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

        conn.perform_window_recovery(now_ms());
        assert!(conn.window > reduced_window);
    }

    #[test]
    fn test_reconnect_logic() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());

        // Injected clock: the whole reconnect-backoff decision is exercised at
        // chosen instants, so the test no longer races two real-clock reads.
        let now = now_ms();

        // Should allow first reconnect attempt
        assert!(conn.should_attempt_reconnect(now));

        // Record attempt
        conn.record_reconnect_attempt(now);
        assert_eq!(conn.reconnection.reconnect_failure_count, 1);

        // Should not allow immediate retry
        assert!(!conn.should_attempt_reconnect(now));

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

        // Stamp last_received CONN_TIMEOUT + 1 seconds in the past.
        conn.last_received = Some(now_ms() - (CONN_TIMEOUT + 1) * 1000);
        assert!(conn.is_timed_out());

        // Disconnected connection should be timed out
        conn.connected = false;
        assert!(conn.is_timed_out());
    }

    /// Deterministic timeout on the single monotonic clock: `is_timed_out` reads
    /// `now_ms()` and compares against the `last_received` stamp, so the test picks
    /// the stamp instead of advancing a virtual clock. A stamp within `CONN_TIMEOUT`
    /// is live; one past the window trips it. Completes in microseconds, no sleep.
    #[test]
    fn monotonic_clock_timeout() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conn = rt.block_on(create_test_connection());
        let now = now_ms();

        conn.last_received = Some(now);
        assert!(!conn.is_timed_out(), "a just-received link is live");

        conn.last_received = Some(now - (CONN_TIMEOUT + 1) * 1000);
        assert!(
            conn.is_timed_out(),
            "a stamp past CONN_TIMEOUT must mark the link timed out"
        );
    }

    /// A freshly created connected link is not timed out: its `last_received` stamp
    /// is essentially `now_ms()`, so the monotonic difference is far below
    /// `CONN_TIMEOUT`. Guards against the timeout tripping on a live link.
    #[test]
    fn fresh_link_not_timed_out_yet() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let conn = rt.block_on(create_test_connection());
        assert!(
            !conn.is_timed_out(),
            "a freshly created link must not be timed out"
        );
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
        assert_eq!(conn.time_since_last_nak_ms(current_time), None);

        conn.register_packet(100, current_time);
        conn.handle_nak(100);
        assert_eq!(conn.congestion.nak_count, 1);
        assert_eq!(conn.congestion.nak_burst_count, 0);
        assert!(conn.time_since_last_nak_ms(now_ms()).is_some());

        let time_since = conn.time_since_last_nak_ms(now_ms()).unwrap();
        assert!(time_since < 1000); // Should be very recent
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
        conn.perform_window_recovery(now_ms());
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
        conn.perform_window_recovery(now_ms());
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
        conn.perform_window_recovery(now_ms());
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
        conn.perform_window_recovery(now_ms());
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
        conn.perform_window_recovery(now_ms());
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
        conn.perform_window_recovery(now_ms());
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
        conn.perform_window_recovery(now_ms());
        // Should NOT recover because increment wait time not met
        assert_eq!(
            conn.window, before,
            "Should not recover when timing constraint not met"
        );

        // Now allow enough time
        conn.congestion.last_window_increase_ms = now_ms() - 1500; // Enough time
        conn.perform_window_recovery(now_ms());
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
        conn.perform_window_recovery(now_ms());
        // Should NOT recover
        assert_eq!(
            conn.window, before_fast,
            "Fast mode should not recover when timing constraint not met"
        );

        // Now allow enough time for fast mode
        conn.congestion.last_window_increase_ms = now_ms() - 400; // Enough for fast mode
        conn.perform_window_recovery(now_ms());
        // Should recover now
        assert!(
            conn.window > before_fast,
            "Fast mode should recover when timing constraint is met"
        );
    }

    /// Build a minimal 16-byte SRT control packet carrying `srt_type` in the
    /// first two bytes (big-endian).
    fn make_srt_control(srt_type: u16) -> [u8; 16] {
        let mut pkt = [0u8; 16];
        pkt[0..2].copy_from_slice(&srt_type.to_be_bytes());
        pkt
    }

    /// Covers the ACK fan-out in `process_connection_events`: an SRT ACK is
    /// broadcast to *every* uplink and is cumulative, so each link clears its own
    /// in-flight packets with seq ≤ ack; a NAK is never broadcast. We first lock
    /// the broadcast *predicate* (an ACK classifies as ACK; a NAK / data packet
    /// does not), then drive the real cumulative `handle_srt_ack`
    /// (`connection/ack_nak.rs`) across a 3-uplink pool and assert in-flight drops
    /// only where seq ≤ ack.
    #[test]
    fn ack_reduces_in_flight() {
        // -- broadcast eligibility predicate --
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

    /// NAKs are attributed to the uplink that originally sent the sequence,
    /// tracked via the `SequenceTracker`. Forward seq S on uplink 1, record it in
    /// the tracker, inject a NAK for S through the production `attribute_nak`, and
    /// assert only uplink 1 is penalized (nak_count++ and in-flight−−); the other
    /// uplinks are untouched.
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

    /// The NAK fallback: when the sequence is *not* tracked the sender scans
    /// uplinks and lets the one still holding it in its packet_log account the
    /// NAK. Because a sequence only ever lives in its real sender's packet_log,
    /// the fallback still lands on the originating uplink. A sequence held by NO
    /// uplink is silently ignored, never double-counted, never a panic.
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

    /// Dedup within the suppression window. Dedup is structural: `handle_nak`
    /// removes the sequence from the packet_log, so a *second* NAK for the same
    /// sequence finds nothing and `handle_nak` returns false (not counted). Inside
    /// the `SequenceTracker` window both NAKs route to the same uplink, and the
    /// attribution short-circuit keeps the duplicate from falling through to
    /// another link. We assert single accounting under a *paused* virtual clock
    /// (no real sleep); the tracker's own window is driven with explicit timestamps.
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
        assert_eq!(
            connections[0].congestion.nak_count, 1,
            "first NAK is counted"
        );
        assert_eq!(
            connections[0].in_flight_packets, 0,
            "first NAK clears the in-flight packet"
        );

        // Advance the paused clock well within the tracking window (no real sleep).
        advance_test_clock(Duration::from_millis(50)).await;
        let within = base + 50;
        // 50ms must be inside the dedup window (checked at compile time).
        const { assert!(50 < SEQUENCE_TRACKING_MAX_AGE_MS) };
        assert_eq!(
            seq_tracker.get(seq, within),
            Some(connections[0].conn_id),
            "the tracker still resolves the sequence to uplink 0 inside the window"
        );

        // Duplicate NAK inside the window: routed to the same uplink, whose
        // packet_log no longer holds the sequence → not re-counted, and the
        // short-circuit keeps the other uplink clean.
        let dup = attribute_nak(&mut connections, &seq_tracker, seq, within);
        assert_eq!(
            dup, None,
            "the duplicate NAK is a no-op (single accounting)"
        );
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
