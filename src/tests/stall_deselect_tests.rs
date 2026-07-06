//! Tests for the stalled-link deselect guard (`stall_deselect`, default on).
//!
//! The guard excludes a link whose in-flight backlog is high while its last
//! delivery proof (earned-ACK or keepalive-RTT sample) has gone stale, but only
//! when a healthier link can carry the traffic. It is a selection penalty only:
//! it never mutates liveness state, and a link recovers on its own once a fresh
//! delivery proof lands (no blind reprobe).

#[cfg(test)]
mod tests {
    use crate::config::{ConfigSnapshot, STALL_ACK_STALE_MS, STALL_MIN_IN_FLIGHT_PACKETS};
    use crate::mode::SchedulingMode;
    use crate::sender::select_connection_idx;
    use crate::test_helpers::create_test_connections;
    use crate::utils::now_ms;

    /// Mark a connection as a stalled black hole at `now`: a backlog at the
    /// stall threshold whose last delivery proof is older than the staleness
    /// window. Kept at exactly the threshold so its raw capacity score
    /// (`window / (in_flight + 1)`) still *beats* a healthier link carrying a
    /// larger backlog — that way a pick against it proves the guard, not score.
    fn make_stalled(conn: &mut crate::connection::SrtlaConnection, now: u64) {
        conn.in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS;
        conn.last_ack_or_rtt_sample_ms = now.saturating_sub(STALL_ACK_STALE_MS + 1000);
    }

    /// A busy-but-healthy link: a larger backlog than [`make_stalled`] (so it
    /// loses on raw score) with a fresh delivery proof (so it is never stalled).
    fn make_healthy_busy(conn: &mut crate::connection::SrtlaConnection, now: u64) {
        conn.in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS * 2;
        conn.last_ack_or_rtt_sample_ms = now;
    }

    fn enhanced() -> ConfigSnapshot {
        ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            ..ConfigSnapshot::default()
        }
    }

    #[test]
    fn stalled_link_is_skipped_when_a_healthy_alternative_exists() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        // Link 0 would win on raw capacity (smaller backlog) but is stalled.
        // Link 1 carries a larger backlog yet is healthy. The guard must pick 1
        // despite link 0's higher raw score — proving it is the guard, not score.
        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);

        let selected = select_connection_idx(&mut conns, None, 0, now, &enhanced());
        assert_eq!(
            selected,
            Some(1),
            "the stalled link must be deselected in favour of the healthy one"
        );
    }

    #[test]
    fn gating_never_mutates_liveness_state() {
        // The whole point of the improved port: no `connected`/`last_received`
        // mask hack. Selection must leave the stalled link's liveness untouched.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        make_stalled(&mut conns[0], now);
        conns[1].in_flight_packets = 4;

        let _ = select_connection_idx(&mut conns, None, 0, now, &enhanced());

        assert!(conns[0].connected, "gating must not clear `connected`");
        assert!(
            conns[0].last_received.is_some(),
            "gating must not clear `last_received`"
        );
        assert!(
            !conns[0].is_timed_out(),
            "a stall-gated link must never be treated as timed out"
        );
    }

    #[test]
    fn all_stalled_falls_back_to_best_never_none() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(3));
        let now = now_ms();

        // Every link is stalled — better to send on a stalled link than to drop
        // the packet. The "any healthy" guard means none get gated.
        for c in conns.iter_mut() {
            make_stalled(c, now);
        }

        let selected = select_connection_idx(&mut conns, None, 0, now, &enhanced());
        assert!(
            selected.is_some(),
            "with every link stalled, selection must still return a link"
        );
    }

    #[test]
    fn a_link_with_no_delivery_proof_yet_is_not_stalled() {
        // in_flight is high but the link has never produced a delivery proof
        // (sample == 0): a fresh burst must not be mistaken for a black hole.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        conns[0].in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS + 8;
        conns[0].last_ack_or_rtt_sample_ms = 0; // no proof yet
        conns[1].in_flight_packets = 4;

        assert!(
            !conns[0].is_stalled(now, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS),
            "a link with no delivery proof yet must not be classed as stalled"
        );
        let _ = select_connection_idx(&mut conns, None, 0, now, &enhanced());
        assert!(!conns[0].stall_gated, "sample==0 link must not be gated");
    }

    #[test]
    fn a_fresh_delivery_proof_ungates_the_link() {
        // Recovery path: no blind reprobe timer. A stale link stamped with a
        // fresh proof (as the keepalive-RTT / earned-ACK sites do) is instantly
        // no longer stalled, even with the backlog still full.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();

        make_stalled(&mut conns[0], now);
        assert!(conns[0].is_stalled(now, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS));

        conns[0].last_ack_or_rtt_sample_ms = now; // fresh keepalive-RTT / ACK
        assert!(
            !conns[0].is_stalled(now, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS),
            "a fresh delivery proof must clear the stall immediately"
        );
    }

    #[test]
    fn guard_off_leaves_selection_unchanged() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        // Link 0 stalled but has the higher raw capacity score (smaller backlog).
        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);

        let config = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: false,
            stall_deselect: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut conns, None, 0, now, &config);
        assert_eq!(
            selected,
            Some(0),
            "with the guard off, the stalled link's raw score must win as before"
        );
        assert!(!conns[0].stall_gated);
    }

    #[test]
    fn classic_mode_also_deselects_stalled_links() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        // Stalled link 0 is the raw-score winner; only the guard demotes it.
        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);

        let config = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut conns, None, 0, now, &config);
        assert_eq!(
            selected,
            Some(1),
            "classic mode must also skip the stalled link when the guard is on"
        );
    }

    #[test]
    fn a_backlog_below_threshold_is_not_stalled() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let conns = rt.block_on(create_test_connections(1));
        let now = now_ms();
        let mut c = conns.into_iter().next().unwrap();

        c.in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS - 1;
        c.last_ack_or_rtt_sample_ms = now.saturating_sub(STALL_ACK_STALE_MS + 1000);
        assert!(
            !c.is_stalled(now, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS),
            "a link below the in-flight threshold must not be stalled regardless of staleness"
        );
    }
}
