#[cfg(test)]
mod tests {
    use crate::sender::selection::rtt_threshold::select_connection;
    use crate::test_helpers::create_test_connections;
    use crate::utils::now_ms;

    #[test]
    fn test_prefers_fast_link() {
        // Two links with different RTTs - should prefer the fast one
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));

        let current_time = now_ms();

        // Connection 0: Low RTT (50ms)
        connections[0].rtt.smooth_rtt_ms = 50.0;
        connections[0].in_flight_packets = 0;

        // Connection 1: High RTT (200ms)
        connections[1].rtt.smooth_rtt_ms = 200.0;
        connections[1].in_flight_packets = 0;

        // With 30ms delta, only connection 0 (50ms) is "fast"
        // Connection 1 (200ms) is above threshold (50 + 30 = 80ms)
        let selected = select_connection(&mut connections, None, 0, current_time, 30, true);

        assert_eq!(selected, Some(0), "Should prefer fast link (low RTT)");
    }

    #[test]
    fn test_both_fast_picks_better_capacity() {
        // Two links both within RTT threshold - picks higher capacity
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));

        let current_time = now_ms();

        // Connection 0: 50ms RTT, lower capacity
        connections[0].rtt.smooth_rtt_ms = 50.0;
        connections[0].in_flight_packets = 5; // Lower score

        // Connection 1: 70ms RTT (within 30ms delta), higher capacity
        connections[1].rtt.smooth_rtt_ms = 70.0;
        connections[1].in_flight_packets = 0; // Higher score

        // Both are "fast" (within 50 + 30 = 80ms), should pick higher capacity
        let selected = select_connection(&mut connections, None, 0, current_time, 30, true);

        assert_eq!(
            selected,
            Some(1),
            "Among fast links, should pick higher capacity"
        );
    }

    #[test]
    fn test_fallback_when_fast_saturated() {
        // Fast link at 0 capacity - should fallback to slow link
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));

        let current_time = now_ms();

        // Connection 0: Fast but saturated (window=0)
        connections[0].rtt.smooth_rtt_ms = 50.0;
        connections[0].window = 0;
        connections[0].in_flight_packets = 10;

        // Connection 1: Slow but has capacity
        connections[1].rtt.smooth_rtt_ms = 200.0;
        connections[1].window = 100;
        connections[1].in_flight_packets = 0;

        let selected = select_connection(&mut connections, None, 0, current_time, 30, true);

        assert_eq!(
            selected,
            Some(1),
            "Should fallback to slow link when fast is saturated"
        );
    }

    #[test]
    fn test_quality_within_fast_links() {
        // Two fast links, one with recent NAKs - should pick cleaner one
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));

        let current_time = now_ms();

        // Connection 0: Fast, equal capacity, but has recent NAKs
        connections[0].rtt.smooth_rtt_ms = 50.0;
        connections[0].in_flight_packets = 0;
        connections[0].congestion.nak_count = 5;
        connections[0].congestion.last_nak_time_ms = current_time - 1000;
        // Set connection established time to beyond startup grace
        connections[0].reconnection.connection_established_ms = current_time - 35000;

        // Connection 1: Fast, equal capacity, no NAKs
        connections[1].rtt.smooth_rtt_ms = 60.0; // Still fast (within delta)
        connections[1].in_flight_packets = 0;
        connections[1].congestion.nak_count = 0;
        // Set connection established time to beyond startup grace
        connections[1].reconnection.connection_established_ms = current_time - 35000;

        // With quality enabled, should prefer connection 1 (no NAKs)
        let selected = select_connection(&mut connections, None, 0, current_time, 30, true);

        assert_eq!(
            selected,
            Some(1),
            "Among fast links, should prefer one with better quality"
        );
    }

    #[test]
    fn test_no_rtt_data_treated_as_fast() {
        // Links without RTT samples should be treated as fast
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));

        let current_time = now_ms();

        // Connection 0: No RTT data (0.0)
        connections[0].rtt.smooth_rtt_ms = 0.0;
        connections[0].in_flight_packets = 5;

        // Connection 1: Has RTT data
        connections[1].rtt.smooth_rtt_ms = 100.0;
        connections[1].in_flight_packets = 0; // Higher capacity

        // Connection 0 should be treated as fast (no RTT data)
        // Both are eligible, should pick based on capacity
        let selected = select_connection(&mut connections, None, 0, current_time, 30, true);

        assert_eq!(
            selected,
            Some(1),
            "Should pick higher capacity when RTT data missing"
        );
    }

    #[test]
    fn test_rtt_threshold_with_large_delta() {
        // With large delta, all links become "fast"
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        let current_time = now_ms();

        // Various RTTs
        connections[0].rtt.smooth_rtt_ms = 50.0;
        connections[0].in_flight_packets = 5;

        connections[1].rtt.smooth_rtt_ms = 150.0;
        connections[1].in_flight_packets = 0; // Best capacity

        connections[2].rtt.smooth_rtt_ms = 200.0;
        connections[2].in_flight_packets = 3;

        // With 200ms delta, all are fast (min 50 + 200 = 250ms threshold)
        let selected = select_connection(&mut connections, None, 0, current_time, 200, true);

        assert_eq!(
            selected,
            Some(1),
            "With large delta, all links fast, should pick best capacity"
        );
    }

    #[test]
    fn test_time_based_dampening() {
        // Should stay with current connection during cooldown
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));

        let current_time = now_ms();
        let last_switch_time = current_time - 200; // 200ms ago (within 500ms cooldown)

        // Connection 0: Currently selected, lower capacity
        connections[0].rtt.smooth_rtt_ms = 50.0;
        connections[0].in_flight_packets = 5;

        // Connection 1: Better link
        connections[1].rtt.smooth_rtt_ms = 50.0;
        connections[1].in_flight_packets = 0;

        let selected = select_connection(
            &mut connections,
            Some(0), // Currently on connection 0
            last_switch_time,
            current_time,
            30,
            true,
        );

        assert_eq!(
            selected,
            Some(0),
            "Should stay with current connection during cooldown"
        );

        // After cooldown, should switch
        let after_cooldown = current_time - 600; // 600ms ago (past cooldown)
        let selected_after = select_connection(
            &mut connections,
            Some(0),
            after_cooldown,
            current_time,
            30,
            true,
        );

        assert_eq!(
            selected_after,
            Some(1),
            "Should switch after cooldown expires"
        );
    }

    #[test]
    fn test_empty_connections() {
        let mut connections: Vec<crate::connection::SrtlaConnection> = vec![];
        let result = select_connection(&mut connections, None, 0, 0, 30, true);
        assert_eq!(result, None, "Should return None for empty connections");
    }

    #[test]
    fn test_all_timed_out() {
        use tokio::time::{Duration, Instant};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));

        // Timeout all connections
        let timeout_instant = Instant::now() - Duration::from_secs(60);
        for conn in &mut connections {
            conn.last_received = Some(timeout_instant);
        }

        let result = select_connection(&mut connections, None, 0, now_ms(), 30, true);
        assert_eq!(
            result, None,
            "Should return None when all connections timed out"
        );
    }

    #[test]
    fn test_quality_disabled() {
        // With quality disabled, should only use base capacity score
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(2));

        let current_time = now_ms();

        // Connection 0: Fast, good capacity, but terrible NAK history
        connections[0].rtt.smooth_rtt_ms = 50.0;
        connections[0].in_flight_packets = 0; // Best capacity
        connections[0].congestion.nak_count = 100;
        connections[0].congestion.last_nak_time_ms = current_time - 100;
        connections[0].reconnection.connection_established_ms = current_time - 35000;

        // Connection 1: Fast, slightly worse capacity, clean history
        connections[1].rtt.smooth_rtt_ms = 50.0;
        connections[1].in_flight_packets = 1;
        connections[1].congestion.nak_count = 0;
        connections[1].reconnection.connection_established_ms = current_time - 35000;

        // With quality disabled, should pick connection 0 (better base capacity)
        let selected = select_connection(&mut connections, None, 0, current_time, 30, false);

        assert_eq!(
            selected,
            Some(0),
            "With quality disabled, should pick based on capacity only"
        );
    }
}
