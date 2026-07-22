//! Tests for the stalled-link deselect guard (`stall_deselect`, default on).
//!
//! The guard excludes a link whose in-flight backlog is high while its last
//! delivery proof (earned-ACK or keepalive-RTT sample) has gone stale, but only
//! when a healthier link can carry the traffic. It is a selection penalty only:
//! it never mutates liveness state. Gating latches asymmetrically: it engages
//! the instant the stall signal fires, and releases only after an
//! uninterrupted run of fresh delivery proof spanning the rejoin dwell (no
//! blind reprobe, no single-sample flap). The staleness window is
//! RTT-adaptive between a floor and the configured ceiling.

#[cfg(test)]
mod tests {
    use srtla_core::mode::SchedulingMode;
    use srtla_core::selection::select_connection_idx;
    use srtla_core::utils::now_ms;

    use crate::config::{ConfigSnapshot, STALL_ACK_STALE_MS, STALL_MIN_IN_FLIGHT_PACKETS};
    use crate::test_helpers::create_test_connections;

    /// Mark a connection as a stalled black hole at `now`: a backlog at the
    /// stall threshold whose last delivery proof is older than the staleness
    /// window. Kept at exactly the threshold so its raw capacity score
    /// (`window / (in_flight + 1)`) still *beats* a healthier link carrying a
    /// larger backlog — that way a pick against it proves the guard, not score.
    fn make_stalled(conn: &mut srtla_core::connection::SrtlaConnection, now: u64) {
        conn.in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS;
        conn.last_ack_or_rtt_sample_ms = now.saturating_sub(STALL_ACK_STALE_MS + 1000);
    }

    /// A busy-but-healthy link: a larger backlog than [`make_stalled`] (so it
    /// loses on raw score) with a fresh delivery proof (so it is never stalled).
    fn make_healthy_busy(conn: &mut srtla_core::connection::SrtlaConnection, now: u64) {
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

        let selected = select_connection_idx(&mut conns, None, now, &enhanced());
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

        let _ = select_connection_idx(&mut conns, None, now, &enhanced());

        assert!(conns[0].connected, "gating must not clear `connected`");
        assert!(
            conns[0].last_received.is_some(),
            "gating must not clear `last_received`"
        );
        assert!(
            !conns[0].is_timed_out(now_ms()),
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

        let selected = select_connection_idx(&mut conns, None, now, &enhanced());
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
        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
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
        let selected = select_connection_idx(&mut conns, None, now, &config);
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
        let selected = select_connection_idx(&mut conns, None, now, &config);
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

    // --- Asymmetric rejoin dwell (librist !375 field lesson: a single fresh
    // sample flaps a still-marginal link back in and re-glitches) ---

    /// Rejoin dwell for a link with no RTT baseline (effective window =
    /// ceiling).
    fn dwell_ms() -> u64 {
        STALL_ACK_STALE_MS * srtla_core::config_snapshot::STALL_REJOIN_DWELL_MULT
    }

    #[test]
    fn rejoin_requires_sustained_proof_not_a_single_sample() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();
        let c = &mut conns[0];

        make_stalled(c, now);
        c.update_stall_latch(now, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(
            c.stall_latched(),
            "latch must engage the instant the stall fires"
        );
        assert_eq!(c.stall_gate_events(), 1);

        // Backlog drains and a single fresh proof lands: not enough.
        c.in_flight_packets = 0;
        c.last_ack_or_rtt_sample_ms = now;
        c.update_stall_latch(now, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(
            c.stall_latched(),
            "a single fresh proof must not release the latch"
        );

        // Proof stays fresh through half the dwell: still latched.
        let mid = now + dwell_ms() / 2;
        c.last_ack_or_rtt_sample_ms = mid;
        c.update_stall_latch(mid, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(c.stall_latched(), "mid-dwell the latch must still hold");

        // Proof sustained for the full dwell: released.
        let done = now + dwell_ms();
        c.last_ack_or_rtt_sample_ms = done;
        c.update_stall_latch(done, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(
            !c.stall_latched(),
            "sustained proof across the dwell must release the latch"
        );
        assert_eq!(
            c.stall_gate_events(),
            1,
            "release must not bump the counter"
        );
    }

    #[test]
    fn proof_lapse_resets_the_rejoin_dwell() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();
        let c = &mut conns[0];

        make_stalled(c, now);
        c.update_stall_latch(now, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        c.in_flight_packets = 0;

        // Recovery run starts...
        let t1 = now + 100;
        c.last_ack_or_rtt_sample_ms = t1;
        c.update_stall_latch(t1, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(c.stall_latched());

        // ...but proof goes stale mid-run (no new sample for a full window).
        let t2 = t1 + STALL_ACK_STALE_MS;
        c.update_stall_latch(t2, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(c.stall_latched(), "stale proof mid-run must keep the latch");

        // A new run starts at t3; even though total elapsed since t1 exceeds
        // the dwell, release counts from t3 — the run must be uninterrupted.
        let t3 = t2 + 100;
        c.last_ack_or_rtt_sample_ms = t3;
        c.update_stall_latch(t3, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        let before = t3 + dwell_ms() - 1;
        c.last_ack_or_rtt_sample_ms = before;
        c.update_stall_latch(before, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(
            c.stall_latched(),
            "the dwell must restart from the new run, not the first sample ever"
        );
        let after = t3 + dwell_ms();
        c.last_ack_or_rtt_sample_ms = after;
        c.update_stall_latch(after, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(!c.stall_latched());
    }

    #[test]
    fn backlog_drain_alone_does_not_release_the_latch() {
        // Cumulative SRT ACKs delivered via the healthy links drain the stalled
        // link's in-flight below the threshold, clearing raw `is_stalled`
        // without the link proving anything. The latch must hold and the link
        // must stay gated in selection.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);

        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].stall_gated);

        // Backlog drains; delivery proof stays ancient. `later` stays inside
        // CONN_TIMEOUT (5 s) so the healthy sibling remains schedulable.
        conns[0].in_flight_packets = 0;
        let later = now + 4000;
        conns[0].last_received = Some(later);
        conns[1].last_received = Some(later);
        conns[1].last_ack_or_rtt_sample_ms = later;
        let selected = select_connection_idx(&mut conns, None, later, &enhanced());
        assert!(
            !conns[0].is_stalled(later, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS),
            "precondition: raw stall signal cleared by the drain"
        );
        assert!(
            conns[0].stall_gated,
            "the latch must keep the drained-but-unproven link gated"
        );
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn disabling_the_guard_clears_the_latch() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);
        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].stall_latched());

        let off = ConfigSnapshot {
            stall_deselect: false,
            ..enhanced()
        };
        let _ = select_connection_idx(&mut conns, None, now, &off);
        assert!(!conns[0].stall_gated, "guard off must clear the flag");
        assert!(!conns[0].stall_latched(), "guard off must clear the latch");
    }

    // --- RTT-adaptive staleness window (librist !375 field lesson: the
    // reaction window is the glitch window) ---

    #[test]
    fn adaptive_stale_window_scales_with_smoothed_rtt() {
        use srtla_core::config_snapshot::STALL_STALE_FLOOR_MS;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let c = &mut conns[0];

        // No RTT baseline yet: fall back to the ceiling.
        assert_eq!(
            c.effective_stall_stale_ms(STALL_ACK_STALE_MS),
            STALL_ACK_STALE_MS
        );

        // Fast link: 4x RTT sits under the floor, so the floor wins — a
        // routine 400-800 ms cellular HARQ stall must never gate a link.
        c.rtt.kalman_rtt.update(50.0);
        assert_eq!(
            c.effective_stall_stale_ms(STALL_ACK_STALE_MS),
            STALL_STALE_FLOOR_MS
        );

        // Mid-range link: pure 4x RTT — reacts in 1.6 s instead of the fixed
        // 3 s the pre-adaptive guard always waited.
        c.rtt.kalman_rtt.reset();
        c.rtt.kalman_rtt.update(400.0);
        assert_eq!(c.effective_stall_stale_ms(STALL_ACK_STALE_MS), 1600);

        // Very slow link: capped at the configured ceiling.
        c.rtt.kalman_rtt.reset();
        c.rtt.kalman_rtt.update(2000.0);
        assert_eq!(
            c.effective_stall_stale_ms(STALL_ACK_STALE_MS),
            STALL_ACK_STALE_MS
        );
    }

    // --- Duplicate-packet probing ---

    #[test]
    fn probe_cadence_is_one_in_n() {
        use srtla_core::config_snapshot::STALL_PROBE_ONE_IN_N;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let c = &mut conns[0];

        let mut fired = 0;
        for _ in 0..(STALL_PROBE_ONE_IN_N * 3) {
            if c.stall_probe_due() {
                fired += 1;
            }
        }
        assert_eq!(fired, 3, "exactly one probe per N routed packets");
    }

    #[tokio::test]
    async fn srtla_ack_credits_the_arrival_link_first() {
        // With duplicate probing the same sequence sits in two links' packet
        // logs (unique copy + probe). The SRTLA ACK returns on the link that
        // delivered, so proof must be stamped on the ARRIVAL link — a
        // first-log-wins scan would let the healthy link's ACK falsely warm the
        // gated link's rejoin dwell.
        use srtla_core::connection::SrtlaIncoming;

        use crate::sender::{SequenceTracker, process_connection_events};

        let mut conns = create_test_connections(2).await;
        let now = now_ms();
        let seq: u32 = 4242;

        // Unique copy on link 0, probe copy on link 1 (gated).
        conns[0].register_packet(seq as i32, now);
        conns[1].register_packet(seq as i32, now);
        conns[0].last_ack_or_rtt_sample_ms = 1;
        conns[1].last_ack_or_rtt_sample_ms = 1;

        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seq_tracker = SequenceTracker::new();
        let incoming = SrtlaIncoming {
            srtla_ack_numbers: smallvec::smallvec![seq],
            read_any: true,
            ..Default::default()
        };

        // ACK arrives on link 1 — the probe link must earn the proof.
        process_connection_events(
            1,
            &mut conns,
            None,
            &listener,
            &seq_tracker,
            false,
            incoming,
        )
        .await
        .unwrap();

        assert!(
            conns[1].last_ack_or_rtt_sample_ms > 1,
            "arrival link must be credited with delivery proof"
        );
        assert_eq!(
            conns[0].last_ack_or_rtt_sample_ms, 1,
            "the other copy's owner must not be falsely credited"
        );
        assert_eq!(
            conns[0].in_flight_packets, 1,
            "the unique copy stays in flight until its own ACK clears it"
        );
        assert_eq!(conns[1].in_flight_packets, 0);
    }

    // --- Cross-mechanism blackout immunity (librist !375 field lesson,
    // commit 8cf11e11: one leg RTT-muted while the other stalled left the
    // balancer with no carrier at all — a self-sustaining both-legs
    // starvation). Two different exclusion mechanisms must never combine
    // into an empty candidate pool. ---

    #[test]
    fn latched_link_with_timed_out_sibling_never_blacks_out() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);
        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].stall_gated);
        assert_eq!(conns[0].stall_gate_events(), 1);

        // The only unlatched sibling dies. The gate must stand down (its
        // "any healthy" guard fails), leaving the latched link selectable —
        // better a stalled carrier than none.
        conns[1].last_received =
            Some(now.saturating_sub(srtla_protocol::CONN_TIMEOUT * 1000 + 1000));
        let selected = select_connection_idx(&mut conns, None, now, &enhanced());
        assert_eq!(
            selected,
            Some(0),
            "with the sibling timed out, the latched link must carry the stream"
        );
        assert!(
            !conns[0].stall_gated,
            "gate must yield when it is the last carrier"
        );
        assert!(
            conns[0].stall_latched(),
            "the latch itself must persist so the link re-gates the moment a carrier returns"
        );

        // Forced carrying must not re-count or re-log the latch every pass
        // (the applied gate is separate state from the latched desire).
        for _ in 0..5 {
            let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        }
        assert_eq!(
            conns[0].stall_gate_events(),
            1,
            "a latch held through carrier-loss fallback must count as ONE engagement"
        );
    }

    #[test]
    fn stall_gate_and_in_flight_cap_never_combine_into_blackout() {
        use srtla_core::selection::enhanced::in_flight_cap_exceeded;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        make_stalled(&mut conns[0], now);
        // Healthy by the latch metric (fresh proof) but over its BDP cap:
        // a tiny CC target caps in-flight at 1 packet against the 64 carried.
        make_healthy_busy(&mut conns[1], now);
        conns[1].cc_target_bps = 100_000;
        assert!(
            in_flight_cap_exceeded(&conns[1]),
            "precondition: sibling must be over its in-flight cap"
        );

        // The stall gate holds (a latch-healthy sibling exists) and the cap
        // gate must yield (no unconstrained link left) — never an empty pool.
        let selected = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].stall_gated);
        assert_eq!(
            selected,
            Some(1),
            "the capped-but-alive link must carry the stream, not an empty pool"
        );
    }
}
