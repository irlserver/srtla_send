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

    // --- Rejoin-dwell backoff (librist !375, 9dc8c406: a drained link always
    // satisfies the rejoin condition, so a fixed dwell oscillates forever) ---

    /// Drive one gate/rejoin cycle: stall the link at `start`, then hold fresh
    /// proof until the latch releases. Returns the instant it released, or
    /// `None` if it was still latched `limit_ms` after the stall.
    fn cycle_until_rejoin(
        c: &mut srtla_core::connection::SrtlaConnection,
        start: u64,
        limit_ms: u64,
    ) -> Option<u64> {
        make_stalled(c, start);
        c.update_stall_latch(start, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(c.stall_latched(), "precondition: the latch engaged");
        // Backlog drains and the link keeps answering probes — the state a
        // held-out link reaches trivially, whatever it can carry under load.
        c.in_flight_packets = 0;
        let mut t = start;
        while t <= start + limit_ms {
            t += 100;
            c.last_ack_or_rtt_sample_ms = t;
            c.update_stall_latch(t, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
            if !c.stall_latched() {
                return Some(t);
            }
        }
        None
    }

    #[test]
    fn a_rejoin_that_immediately_re_stalls_doubles_the_dwell() {
        // The hole this closes: the dwell only asks for sustained delivery
        // proof, which a drained link supplies from its probes alone. A link
        // that cannot carry its share rejoined, reflooded and re-gated on a
        // fixed ~6s period for as long as the path stayed marginal, each cycle
        // costing a payload-path transition.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();
        let c = &mut conns[0];

        // First cycle runs at the base dwell and is free of any penalty.
        let first = cycle_until_rejoin(c, now, dwell_ms() * 4).expect("first rejoin");
        assert!(
            first - now <= dwell_ms() + 200,
            "the first rejoin must use the base dwell"
        );
        assert_eq!(c.stall_rejoin_backoff(), 1, "the first gate is free");

        // It re-stalls at once — well inside the probation window.
        let second = cycle_until_rejoin(c, first + 100, dwell_ms() * 8).expect("second rejoin");
        assert_eq!(
            c.stall_rejoin_backoff(),
            2,
            "a rejoin that did not hold must double the dwell"
        );
        assert!(
            second - first > dwell_ms(),
            "the second rejoin must wait longer than the base dwell: took {}ms",
            second - first
        );

        // And again: the escalation compounds rather than resetting.
        let third = cycle_until_rejoin(c, second + 100, dwell_ms() * 16).expect("third rejoin");
        assert_eq!(c.stall_rejoin_backoff(), 4);
        assert!(
            third - second > 2 * dwell_ms(),
            "the third wait must exceed the doubled dwell: took {}ms",
            third - second
        );
    }

    #[test]
    fn a_rejoin_that_holds_clears_the_backoff() {
        // A link recovering from a transient spike must not keep paying for an
        // earlier failure, or one bad patch would exile it for minutes.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();
        let c = &mut conns[0];

        let first = cycle_until_rejoin(c, now, dwell_ms() * 4).expect("first rejoin");
        let second = cycle_until_rejoin(c, first + 100, dwell_ms() * 8).expect("second rejoin");
        assert_eq!(c.stall_rejoin_backoff(), 2, "precondition: escalated");

        // This time the rejoin outlasts probation before the link stalls again.
        let probation_ms =
            STALL_ACK_STALE_MS * srtla_core::config_snapshot::STALL_REJOIN_PROBATION_MULT;
        let late = second + probation_ms + 1000;
        let third = cycle_until_rejoin(c, late, dwell_ms() * 4).expect("third rejoin");
        assert_eq!(
            c.stall_rejoin_backoff(),
            1,
            "a rejoin that held must clear the escalation"
        );
        assert!(
            third - late <= dwell_ms() + 200,
            "and the next dwell must be back to the base: took {}ms",
            third - late
        );
    }

    #[test]
    fn the_rejoin_backoff_saturates() {
        // Still retried, just rarely — an exiled link must never be abandoned.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let mut t = now_ms();
        let c = &mut conns[0];

        for _ in 0..8 {
            t = cycle_until_rejoin(c, t + 100, dwell_ms() * 64).expect("rejoin still happens")
        }
        assert_eq!(
            c.stall_rejoin_backoff(),
            srtla_core::config_snapshot::STALL_REJOIN_BACKOFF_MAX,
            "the multiplier must stop at the ceiling"
        );
    }

    #[test]
    fn a_long_wait_does_not_stretch_the_share_ramp() {
        // Only the wait scales. How gently a link is reloaded is a property of
        // the link, not of how long it sat out; scaling the ramp too would pin
        // a recovered link at the ramp floor for minutes.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();
        let c = &mut conns[0];

        let first = cycle_until_rejoin(c, now, dwell_ms() * 4).expect("first rejoin");
        let second = cycle_until_rejoin(c, first + 100, dwell_ms() * 8).expect("second rejoin");
        assert_eq!(c.stall_rejoin_backoff(), 2, "precondition: escalated");

        // The ramp still completes one base dwell after the (delayed) rejoin.
        assert!(
            c.is_rejoin_ramping(second + dwell_ms() - 100),
            "the ramp must still be running just short of the base dwell"
        );
        assert!(
            !c.is_rejoin_ramping(second + dwell_ms()),
            "and must be done at the base dwell, not the backed-off one"
        );
    }

    // --- Timeliness of the proof itself (librist !375, 6ed9d2a3: delivery on a
    // leg queued deeper than the receiver's buffer is not evidence of anything) ---

    /// Settles the smoothed RTT and the recent minimum alike, as real samples would.
    fn set_rtt(conn: &mut srtla_core::connection::SrtlaConnection, rtt_ms: f64) {
        let t0 = now_ms();
        for i in 0..16 {
            conn.rtt.update_estimate(rtt_ms as u64, t0 + i);
        }
        assert!(
            (conn.get_smooth_rtt_ms() - rtt_ms).abs() < rtt_ms * 0.1,
            "the RTT estimator must have converged for the test to mean anything"
        );
    }

    fn with_budget(budget_ms: u32) -> ConfigSnapshot {
        ConfigSnapshot {
            negotiated_latency_ms: budget_ms,
            ..enhanced()
        }
    }

    /// Gate a link at `rtt_ms`, then feed it an unbroken run of fresh delivery
    /// proof for three dwells through the real selection path. Returns whether
    /// the latch ever released.
    fn rejoins_at(rtt_ms: f64, budget_ms: u32) -> bool {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        let config = with_budget(budget_ms);

        set_rtt(&mut conns[0], rtt_ms);
        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);
        let _ = select_connection_idx(&mut conns, None, now, &config);
        assert!(conns[0].stall_latched(), "precondition: the latch engaged");

        // The state a held-out link reaches for free: backlog drained, probes
        // and keepalives answered, delivery proof fresh every tick.
        conns[0].in_flight_packets = 0;
        let mut t = now;
        while t <= now + dwell_ms() * 3 {
            t += 100;
            conns[0].last_ack_or_rtt_sample_ms = t;
            conns[1].last_ack_or_rtt_sample_ms = t;
            let _ = select_connection_idx(&mut conns, None, t, &config);
            if !conns[0].stall_latched() {
                return true;
            }
        }
        false
    }

    #[test]
    fn keepalive_echoes_alone_release_a_low_rtt_link() {
        // Issue #25: a latched link carries no payload, so its steady proof is
        // one keepalive echo per second. On a 20 ms link the bare staleness
        // window is its 1 s floor, and each echo used to land just after the
        // previous one went stale, so the dwell never completed.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();
        let c = &mut conns[0];

        set_rtt(c, 20.0);
        let stale = c.effective_stall_stale_ms(STALL_ACK_STALE_MS);
        assert_eq!(stale, srtla_core::config_snapshot::STALL_STALE_FLOOR_MS);
        make_stalled(c, now);
        c.update_stall_latch(now, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
        assert!(c.stall_latched(), "precondition: the latch engaged");
        c.in_flight_packets = 0;

        let base_dwell = stale * srtla_core::config_snapshot::STALL_REJOIN_DWELL_MULT;
        let mut next_keepalive = now;
        let mut pending_echo: Option<([u8; srtla_protocol::SRTLA_KEEPALIVE_EXT_LEN], u64)> = None;
        let mut k = 0u64;
        let mut t = now;
        while t <= now + base_dwell * 3 {
            t += 1;
            if t >= next_keepalive {
                // Each keepalive leaves a few ms after its tick deadline and
                // each round trip varies by a few ms, as on any real path, so
                // consecutive echoes are not exactly one period apart.
                let sent = t + (k % 3) * 4;
                let rtt = 20 + (k % 2) * 3;
                pending_echo = Some((c.keepalive_packet(sent), sent + rtt));
                next_keepalive += 1000;
                k += 1;
            }
            if let Some((echo, at)) = pending_echo
                && t >= at
            {
                c.handle_keepalive_echo(&echo, t);
                pending_echo = None;
            }
            c.update_stall_latch(t, STALL_MIN_IN_FLIGHT_PACKETS, STALL_ACK_STALE_MS);
            if !c.stall_latched() {
                return;
            }
        }
        panic!("a link answering every keepalive must rejoin within three dwells");
    }

    #[test]
    fn proof_from_a_link_slower_than_the_buffer_does_not_count() {
        // 2s round trip is 1s one way. Against a 500ms receive buffer nothing
        // this link delivers can be played, so the fresh proof it keeps
        // producing — keepalive echoes cost it nothing — is not evidence that
        // it can carry the stream again.
        assert!(
            !rejoins_at(2000.0, 500),
            "a link that cannot beat the deadline must not clear the dwell"
        );
    }

    #[test]
    fn the_same_link_rejoins_when_the_buffer_can_absorb_it() {
        // Identical link and identical proof; only the receiver's buffer
        // differs. 1s of one-way delay fits inside 4s, so the dwell decides as
        // it always has.
        assert!(rejoins_at(2000.0, 4000));
    }

    #[test]
    fn an_undeclared_buffer_leaves_the_dwell_untouched() {
        // No handshake seen (or a peer without TSBPD): there is nothing to
        // measure against, so withholding proof would strand the link forever.
        assert!(rejoins_at(2000.0, 0));
    }

    #[test]
    fn a_link_that_speeds_back_up_is_let_in() {
        // Nothing here stops RTT being measured, so a path that genuinely
        // recovers crosses back over the bar on its own — the link is still
        // retried, it just stops being retried on evidence it never earned.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        let config = with_budget(500);

        set_rtt(&mut conns[0], 2000.0);
        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);
        let _ = select_connection_idx(&mut conns, None, now, &config);
        assert!(conns[0].stall_latched());

        conns[0].in_flight_packets = 0;
        let mut t = now;
        for _ in 0..(dwell_ms() * 2 / 100) {
            t += 100;
            conns[0].last_ack_or_rtt_sample_ms = t;
            conns[1].last_ack_or_rtt_sample_ms = t;
            let _ = select_connection_idx(&mut conns, None, t, &config);
        }
        assert!(
            conns[0].stall_latched(),
            "two dwells of untimely proof must not add up to a rejoin"
        );

        // The path clears. 200ms round trip is 100ms one way, inside the 500ms
        // buffer with room to spare.
        set_rtt(&mut conns[0], 200.0);
        let deadline = t + dwell_ms() * 3;
        while t <= deadline {
            t += 100;
            conns[0].last_ack_or_rtt_sample_ms = t;
            conns[1].last_ack_or_rtt_sample_ms = t;
            let _ = select_connection_idx(&mut conns, None, t, &config);
            if !conns[0].stall_latched() {
                return;
            }
        }
        panic!("a recovered link must rejoin once its proof is timely again");
    }

    #[test]
    fn timeliness_is_measured_one_way_against_the_buffer() {
        use srtla_core::connection::delivery_proof_is_timely;

        // Half the round trip has to fit: 1000ms one way exactly fills a
        // 1000ms buffer, 1001ms does not.
        assert!(delivery_proof_is_timely(2000.0, 1000));
        assert!(!delivery_proof_is_timely(2002.0, 1000));

        // No declared buffer, and no RTT baseline yet, are both "nothing to
        // check against" rather than grounds to withhold proof.
        assert!(delivery_proof_is_timely(9999.0, 0));
        assert!(delivery_proof_is_timely(0.0, 10));
    }

    #[test]
    fn one_batch_delayed_ack_does_not_hold_a_recovered_link_out() {
        use srtla_core::connection::delivery_proof_is_timely;

        // A receiver that batches SRTLA ACKs holds the ACK for a packet sent
        // just before an outage until traffic resumes, so one sample comes back
        // as long as the outage. It drags the smoothed RTT past the buffer,
        // but the 80 ms path is still there in the recent floor.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let c = &mut conns[0];
        let t0 = 1_000_000;
        for i in 0..16 {
            c.rtt.update_estimate(80, t0 + i * 100);
        }
        c.rtt.update_estimate(1_500, t0 + 1_700);

        assert!(
            c.get_smooth_rtt_ms() / 2.0 > 260.0,
            "the delayed sample must push the smoothed RTT past the buffer, got {}",
            c.get_smooth_rtt_ms()
        );
        assert!(c.rejoin_rtt_ms() <= 81.0, "got {}", c.rejoin_rtt_ms());
        assert!(delivery_proof_is_timely(c.rejoin_rtt_ms(), 260));
    }

    #[test]
    fn a_latched_link_rejoins_despite_an_inflated_smoothed_rtt() {
        // The batch-delayed sample above, through the real latch: the
        // smoothed RTT says the link misses a 260 ms buffer, the recent floor
        // says it is an 80 ms path, and the floor decides.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let t0 = now_ms();
        let config = with_budget(260);

        for i in 0..16 {
            conns[0].rtt.update_estimate(80, t0 + i * 100);
        }
        conns[0].rtt.update_estimate(1_500, t0 + 1_700);
        assert!(
            conns[0].get_smooth_rtt_ms() / 2.0 > 260.0,
            "precondition: the smoothed RTT misses the buffer, got {}",
            conns[0].get_smooth_rtt_ms()
        );

        let now = t0 + 1_800;
        make_stalled(&mut conns[0], now);
        make_healthy_busy(&mut conns[1], now);
        let _ = select_connection_idx(&mut conns, None, now, &config);
        assert!(conns[0].stall_latched(), "precondition: the latch engaged");

        conns[0].in_flight_packets = 0;
        let mut t = now;
        while t <= now + dwell_ms() * 3 {
            t += 100;
            conns[0].last_ack_or_rtt_sample_ms = t;
            conns[1].last_ack_or_rtt_sample_ms = t;
            let _ = select_connection_idx(&mut conns, None, t, &config);
            if !conns[0].stall_latched() {
                return;
            }
        }
        panic!("an 80 ms path must rejoin even while one delayed ACK inflates its smoothed RTT");
    }

    #[test]
    fn a_link_with_only_late_samples_is_still_judged_late() {
        use srtla_core::connection::delivery_proof_is_timely;

        // With no fast sample in the window the floor is as late as the link.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let c = &mut conns[0];
        let t0 = 1_000_000;
        for i in 0..16 {
            c.rtt.update_estimate(900, t0 + i * 100);
        }

        assert!(!delivery_proof_is_timely(c.rejoin_rtt_ms(), 260));
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

    // --- Post-rejoin share ramp (librist !375 afdc7ed6: a leg that rejoins at
    // full weight re-floods the queue it drained while gated and re-mutes
    // seconds later, oscillating for as long as the link stays marginal) ---

    /// Route `count` packets and return how many each link won. Mirrors the
    /// real feedback loop: routing a packet raises the winner's in-flight
    /// count, which lowers its own score for the next one.
    fn route(
        conns: &mut [srtla_core::connection::SrtlaConnection],
        now: u64,
        count: usize,
    ) -> (usize, usize) {
        let mut picks = (0usize, 0usize);
        for _ in 0..count {
            // `last_idx: None` on every packet so the result is pure score,
            // with no switch hysteresis mixed in.
            match select_connection_idx(conns, None, now, &enhanced()) {
                Some(0) => {
                    picks.0 += 1;
                    conns[0].in_flight_packets += 1;
                }
                Some(1) => {
                    picks.1 += 1;
                    conns[1].in_flight_packets += 1;
                }
                other => panic!("selection returned {other:?} with two usable links"),
            }
        }
        picks
    }

    /// Drive link 0 through a full gate-and-release cycle and leave both links
    /// at the moment of release: link 0 drained (as gating guarantees), link 1
    /// carrying the stream. Returns that timestamp.
    fn gate_then_release(conns: &mut [srtla_core::connection::SrtlaConnection], t0: u64) -> u64 {
        make_stalled(&mut conns[0], t0);
        make_healthy_busy(&mut conns[1], t0);
        let _ = select_connection_idx(conns, None, t0, &enhanced());
        assert!(conns[0].stall_gated, "precondition: link 0 gated");

        // Gated means no unique payload, so the backlog drains to nothing.
        conns[0].in_flight_packets = 0;

        // Fresh delivery proof, sustained across the rejoin dwell.
        let run_start = t0 + 100;
        for c in conns.iter_mut() {
            c.last_received = Some(run_start);
            c.last_ack_or_rtt_sample_ms = run_start;
        }
        let _ = select_connection_idx(conns, None, run_start, &enhanced());
        assert!(conns[0].stall_latched(), "one sample must not release");

        let released = run_start + dwell_ms();
        for c in conns.iter_mut() {
            c.last_received = Some(released);
            c.last_ack_or_rtt_sample_ms = released;
        }
        let _ = select_connection_idx(conns, None, released, &enhanced());
        assert!(!conns[0].stall_latched(), "sustained proof must release");
        released
    }

    #[test]
    fn rejoining_link_ramps_its_share_instead_of_seizing_the_stream() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let released = gate_then_release(&mut conns, now_ms());

        assert!(
            conns[0].is_rejoin_ramping(released),
            "releasing the latch must arm the share ramp"
        );

        // Both links start this round where the gate left them: link 0 drained
        // by the gating, link 1 carrying the whole stream. On raw score that
        // makes the *rejoining* link look 65x better than the one actually
        // doing the work — the artefact the ramp exists to neutralise.
        conns[0].in_flight_packets = 0;
        conns[1].in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS * 2;
        let (rejoiner, incumbent) = route(&mut conns, released, 60);

        assert!(
            rejoiner < incumbent,
            "the rejoining link must not take the stream off the working one (rejoiner \
             {rejoiner}, incumbent {incumbent})"
        );
        assert!(
            rejoiner > 0,
            "it must still carry something, or it can never prove itself"
        );
    }

    #[test]
    fn the_ramp_expires_and_the_link_competes_at_full_score() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let released = gate_then_release(&mut conns, now_ms());

        // Same starting state, but the ramp has run its course.
        let done = released + dwell_ms();
        for c in conns.iter_mut() {
            c.last_received = Some(done);
            c.last_ack_or_rtt_sample_ms = done;
        }
        assert!(
            !conns[0].is_rejoin_ramping(done),
            "the ramp must expire on its own"
        );

        conns[0].in_flight_packets = 0;
        conns[1].in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS * 2;
        let (rejoiner, incumbent) = route(&mut conns, done, 60);

        assert!(
            rejoiner > incumbent,
            "with the ramp expired the recovered link competes on raw capacity again (rejoiner \
             {rejoiner}, incumbent {incumbent})"
        );
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

    #[test]
    fn an_srtla_ack_proves_the_link_it_arrived_on() {
        // Link 0 has the much slower return path. The cumulative SRT ACK that
        // came back over link 1 already pruned link 0's packet, and a later
        // retransmit of it went over link 1, so the tracker now names link 1.
        // Link 0's own SRTLA ACK for it matches nothing there, but still
        // proves link 0.
        use srtla_core::connection::SrtlaIncoming;

        use crate::sender::{SequenceTracker, apply_link_feedback};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        let seq: u32 = 500;
        send_payload(&mut conns[0], seq, 1, now);
        let mut seq_tracker = SequenceTracker::new();
        seq_tracker.insert(seq, conns[0].conn_id, now);
        conns[0].handle_srt_ack(seq as i32, now + 5, false);
        assert_eq!(conns[0].in_flight_packets, 0);
        send_payload(&mut conns[1], seq, 1, now + 5);
        seq_tracker.insert(seq, conns[1].conn_id, now + 5);
        for c in conns.iter_mut() {
            c.last_ack_or_rtt_sample_ms = 1;
        }
        assert_eq!(conns[0].unproven_sends, 1);
        let ack = SrtlaIncoming {
            srtla_ack_numbers: smallvec::smallvec![seq],
            read_any: true,
            ..Default::default()
        };

        apply_link_feedback(0, &mut conns, &seq_tracker, false, &ack, now + 150);
        assert_eq!(conns[0].last_ack_or_rtt_sample_ms, now + 150);
        assert_eq!(conns[0].unproven_sends, 0);
        assert_eq!(
            conns[1].last_ack_or_rtt_sample_ms, 1,
            "the arrival link earns the proof, not the link the tracker names"
        );

        // The same ACK arriving on link 1 is link 1's proof, and only its.
        conns[0].last_ack_or_rtt_sample_ms = 1;
        apply_link_feedback(1, &mut conns, &seq_tracker, false, &ack, now + 160);
        assert_eq!(conns[1].last_ack_or_rtt_sample_ms, now + 160);
        assert_eq!(conns[0].last_ack_or_rtt_sample_ms, 1);
    }

    #[test]
    fn before_reg3_an_unmatched_srtla_ack_proves_nothing_and_a_matched_one_proves_its_packet() {
        // The parser hands over SRTLA ACK entries for a link the sender has not
        // seen REG3 for yet. An entry whose packet the log no longer holds must
        // not stamp the proof the unproven-send rule reads. One that matches a
        // packet the link sent proves it through the per-sequence path, as it
        // did before the arrival-link rule. Neither counts as an arrival report.
        use srtla_core::connection::SrtlaIncoming;

        use crate::sender::{SequenceTracker, apply_link_feedback};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        conns[0].connected = false;
        conns[0].last_ack_or_rtt_sample_ms = 1;
        send_payload(&mut conns[0], 500, 1, now);
        send_payload(&mut conns[0], 600, 1, now);
        let mut seq_tracker = SequenceTracker::new();
        seq_tracker.insert(500, conns[0].conn_id, now);
        seq_tracker.insert(600, conns[0].conn_id, now);
        // Sequence 500 left the log through a cumulative ACK; 600 is still in it.
        conns[0].handle_srt_ack(501, now + 5, false);
        assert_eq!(conns[0].in_flight_packets, 1);
        let ack = |seq: u32| SrtlaIncoming {
            srtla_ack_numbers: smallvec::smallvec![seq],
            read_any: true,
            ..Default::default()
        };

        apply_link_feedback(0, &mut conns, &seq_tracker, false, &ack(500), now + 50);
        assert_eq!(
            (conns[0].last_ack_or_rtt_sample_ms, conns[0].unproven_sends),
            (1, 2),
            "an unmatched ACK before REG3 is no proof"
        );

        apply_link_feedback(0, &mut conns, &seq_tracker, false, &ack(600), now + 60);
        assert_eq!(
            conns[0].in_flight_packets, 0,
            "a matched ACK before REG3 still settles its packet"
        );
        assert_eq!(
            (conns[0].last_ack_or_rtt_sample_ms, conns[0].unproven_sends),
            (now + 60, 0),
            "a matched ACK before REG3 proves the packet it settles"
        );
        assert_eq!(
            (
                conns[0].srtla_ack_entries_total(),
                conns[0].last_srtla_ack_age_ms(now + 60)
            ),
            (0, None),
            "no ACK before REG3 is an arrival report"
        );

        conns[0].connected = true;
        send_payload(&mut conns[0], 700, 1, now + 70);
        conns[0].last_ack_or_rtt_sample_ms = 1;
        apply_link_feedback(0, &mut conns, &seq_tracker, false, &ack(500), now + 80);
        assert_eq!(
            (conns[0].last_ack_or_rtt_sample_ms, conns[0].unproven_sends),
            (now + 80, 0),
            "once registered, the same unmatched ACK proves the link"
        );
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

    // --- Fast silence pull (librist c7a578d1's fast routing tier: pull a
    // briefly-mute loaded link right now, readmit the moment it speaks;
    // sustained silence escalates into the sticky latch) ---

    #[test]
    fn loaded_silent_link_is_pulled_and_readmitted_when_it_speaks() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        // Loaded, delivery proof fresh (so the latch is out of the picture),
        // but nothing received for longer than the pull window.
        conns[0].in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS;
        conns[0].last_ack_or_rtt_sample_ms = now;
        conns[0].last_received = Some(now - 300);
        make_healthy_busy(&mut conns[1], now);

        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].stall_gated, "a loaded mute link must be pulled");
        assert!(!conns[0].stall_latched(), "the fast tier must not latch");
        assert_eq!(conns[0].silence_pulls(), 1);

        // Any inbound byte readmits it instantly — no dwell on the fast tier.
        conns[0].last_received = Some(now);
        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(
            !conns[0].stall_gated,
            "a byte must clear the pull instantly"
        );

        // A second stall counts as a second engagement.
        conns[0].last_received = Some(now - 300);
        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].stall_gated);
        assert_eq!(conns[0].silence_pulls(), 2);
    }

    #[test]
    fn idle_link_keepalive_gaps_are_never_pulled() {
        // An idle link's only inbound is the 1 s keepalive echo; a 900 ms gap
        // is routine there and must never read as a stall.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        conns[0].in_flight_packets = 0;
        conns[0].last_received = Some(now - 900);
        make_healthy_busy(&mut conns[1], now);

        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(!conns[0].stall_gated, "idle links are never silence-pulled");
        assert_eq!(conns[0].silence_pulls(), 0);
    }

    #[test]
    fn silence_window_scales_with_rtt_and_caps_at_stale_window() {
        use srtla_core::config_snapshot::SILENCE_PULL_FLOOR_MS;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let c = &mut conns[0];

        // No RTT baseline: the floor.
        assert_eq!(
            c.silence_pull_window_ms(STALL_ACK_STALE_MS),
            SILENCE_PULL_FLOOR_MS
        );
        // Fast link: 2x RTT sits under the floor, floor wins.
        c.rtt.kalman_rtt.update(50.0);
        assert_eq!(
            c.silence_pull_window_ms(STALL_ACK_STALE_MS),
            SILENCE_PULL_FLOOR_MS
        );
        // Satellite-class link: 2x RTT, so a silence shorter than the path's
        // own round trip never pulls.
        c.rtt.kalman_rtt.reset();
        c.rtt.kalman_rtt.update(300.0);
        assert_eq!(c.silence_pull_window_ms(STALL_ACK_STALE_MS), 600);
        // Beyond the staleness window the latch owns the decision.
        c.rtt.kalman_rtt.reset();
        c.rtt.kalman_rtt.update(2000.0);
        assert_eq!(
            c.silence_pull_window_ms(STALL_ACK_STALE_MS),
            STALL_ACK_STALE_MS
        );
    }

    #[test]
    fn pull_holds_through_backlog_drain_and_escalates_to_latch() {
        // Under a real blackhole, cumulative SRT ACKs (via the healthy link)
        // drain the pulled link's backlog within an RTT. The pull must HOLD
        // through that drain (releasing on drain would refill the hole in a
        // duty cycle), and sustained silence must escalate into the latch so
        // recovery goes through the dwell and probes, not a cold readmit.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();

        conns[0].in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS;
        conns[0].last_received = Some(now - 300);
        conns[0].last_ack_or_rtt_sample_ms = now - 300;
        make_healthy_busy(&mut conns[1], now);

        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].stall_gated && !conns[0].stall_latched());

        // Backlog drains; the link is still mute. The pull must hold.
        conns[0].in_flight_packets = 0;
        let t1 = now + 500;
        conns[1].last_received = Some(t1);
        conns[1].last_ack_or_rtt_sample_ms = t1;
        let _ = select_connection_idx(&mut conns, None, t1, &enhanced());
        assert!(
            conns[0].stall_gated,
            "the pull must hold through a backlog drain, not readmit a mute link"
        );
        assert!(!conns[0].stall_latched());

        // Proof goes fully stale while the pull is held: escalate to latch.
        let t2 = now + STALL_ACK_STALE_MS;
        conns[1].last_received = Some(t2);
        conns[1].last_ack_or_rtt_sample_ms = t2;
        let _ = select_connection_idx(&mut conns, None, t2, &enhanced());
        assert!(
            conns[0].stall_latched(),
            "sustained silence must escalate the pull into the sticky latch"
        );
        assert_eq!(conns[0].stall_gate_events(), 1);

        // The link speaks: the pull clears but the LATCH governs readmission
        // (dwell), so one byte no longer flaps payload back onto it.
        conns[0].last_received = Some(t2);
        conns[0].last_ack_or_rtt_sample_ms = t2;
        let _ = select_connection_idx(&mut conns, None, t2, &enhanced());
        assert!(
            conns[0].stall_gated,
            "after escalation, a single byte must not readmit the link"
        );
    }

    // --- Unproven sends: payload handed to a link since its last proof ---

    /// Route `n` payload packets onto `conn` through the real send path.
    fn send_payload(
        conn: &mut srtla_core::connection::SrtlaConnection,
        first: u32,
        n: u32,
        now: u64,
    ) {
        for seq in first..first + n {
            let mut pkt = [0u8; 16];
            pkt[..4].copy_from_slice(&seq.to_be_bytes());
            conn.queue_data_packet(&pkt, Some(seq), now);
        }
        let _ = conn.take_batch(now);
    }

    /// Route `n` payload packets onto `conn` at `at`, then let a cumulative
    /// SRT ACK (arriving over a sibling) clear them from its log: the link
    /// owes their proof, but carries no backlog.
    fn send_drained(
        conn: &mut srtla_core::connection::SrtlaConnection,
        first: u32,
        n: u32,
        at: u64,
    ) {
        send_payload(conn, first, n, at);
        conn.handle_srt_ack((first + n - 1) as i32, at, false);
        assert_eq!(conn.in_flight_packets, 0);
    }

    #[test]
    fn a_dead_link_drained_by_naks_and_acks_still_stalls() {
        // The field failure: NAKs and cumulative SRT ACKs reach the sender
        // over the healthy link and prune the dead link's packet log, so its
        // backlog never reaches the in-flight threshold. What it was handed
        // since its last proof does not drain.
        use srtla_core::config_snapshot::STALL_MIN_UNPROVEN_SENDS;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        conns[0].last_ack_or_rtt_sample_ms = now.saturating_sub(STALL_ACK_STALE_MS + 1000);
        send_payload(&mut conns[0], 100, STALL_MIN_UNPROVEN_SENDS, now - 1_000);
        for seq in 100..110 {
            conns[0].handle_nak(seq, false, now);
        }
        conns[0].handle_srt_ack(100 + STALL_MIN_UNPROVEN_SENDS as i32 - 1, now, false);
        assert_eq!(conns[0].in_flight_packets, 0, "the backlog has drained");
        make_healthy_busy(&mut conns[1], now);

        let selected = select_connection_idx(&mut conns, None, now, &enhanced());
        assert_eq!(selected, Some(1));
        assert!(conns[0].stall_gated, "unproven payload must gate the link");
        assert!(conns[0].stall_latched());
    }

    #[test]
    fn unproven_payload_waits_out_a_late_keepalive_echo() {
        // A link whose only steady proof is the 1 s keepalive echo: one echo
        // 100 ms late must not hold it out, one missing echo plus margin must.
        use srtla_core::config_snapshot::{
            STALL_MIN_UNPROVEN_SENDS, STALL_UNPROVEN_STALE_FLOOR_MS,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        conns[0].rtt.update_estimate(7, now);
        send_drained(&mut conns[0], 100, STALL_MIN_UNPROVEN_SENDS, now - 1_000);
        make_healthy_busy(&mut conns[1], now);

        conns[0].last_ack_or_rtt_sample_ms = now - 1_100;
        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(
            !conns[0].stall_gated,
            "an echo 100 ms late is jitter, not a dead path"
        );

        conns[0].last_ack_or_rtt_sample_ms = now - STALL_UNPROVEN_STALE_FLOOR_MS;
        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].stall_gated);
    }

    #[test]
    fn a_pull_on_unproven_sends_alone_waits_for_the_floor_and_releases_on_an_echo() {
        // Two live 20 ms links. Link 0 sends a full count over 120 ms, link
        // 1's cumulative ACK clears it at 140 ms, then link 0's return traffic
        // pauses until a keepalive echo at 1.1 s. The fast pull may take it out
        // for that gap, but the gap is not a dead path: no latch, and the echo
        // gives it back at once.
        use srtla_core::config_snapshot::STALL_MIN_UNPROVEN_SENDS;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let t = now_ms();
        conns[0].rtt.update_estimate(20, t);
        conns[0].last_ack_or_rtt_sample_ms = t;
        for i in 0..STALL_MIN_UNPROVEN_SENDS {
            send_payload(
                &mut conns[0],
                100 + i,
                1,
                t + (i as u64 * 120) / STALL_MIN_UNPROVEN_SENDS as u64,
            );
        }
        conns[0].handle_srt_ack((100 + STALL_MIN_UNPROVEN_SENDS - 1) as i32, t + 140, false);
        conns[0].last_received = Some(t + 140);
        assert_eq!(conns[0].in_flight_packets, 0);

        let select_at = |conns: &mut smallvec::SmallVec<_, 4>, at: u64| {
            make_healthy_busy(&mut conns[1], at);
            conns[1].last_received = Some(at);
            let _ = select_connection_idx(conns, None, at, &enhanced());
        };
        select_at(&mut conns, t + 400);
        assert!(
            conns[0].stall_gated,
            "the fast pull takes the silent link out"
        );
        assert_eq!(conns[0].silence_pulls(), 1);

        select_at(&mut conns, t + 1_000);
        assert!(
            !conns[0].stall_latched(),
            "a pull on unproven sends alone must not latch before the floor"
        );

        let ka = conns[0].keepalive_packet(t + 1_080);
        conns[0].handle_keepalive_echo(&ka, t + 1_100);
        conns[0].last_received = Some(t + 1_100);
        select_at(&mut conns, t + 1_100);
        assert!(!conns[0].stall_gated, "the echo gives the link back");
        assert_eq!(conns[0].stall_gate_events(), 0);
        assert_eq!(conns[0].silence_pulls(), 1);
    }

    #[test]
    fn a_pull_on_a_full_backlog_keeps_the_rtt_window_after_draining() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let t = now_ms();
        conns[0].rtt.update_estimate(20, t);
        conns[0].last_ack_or_rtt_sample_ms = t;
        send_payload(&mut conns[0], 100, STALL_MIN_IN_FLIGHT_PACKETS as u32, t);
        conns[0].last_received = Some(t);
        make_healthy_busy(&mut conns[1], t + 300);
        conns[1].last_received = Some(t + 300);
        let _ = select_connection_idx(&mut conns, None, t + 300, &enhanced());
        assert!(conns[0].stall_gated);

        conns[0].handle_srt_ack(100 + STALL_MIN_IN_FLIGHT_PACKETS - 1, t + 500, false);
        make_healthy_busy(&mut conns[1], t + 1_000);
        conns[1].last_received = Some(t + 1_000);
        let _ = select_connection_idx(&mut conns, None, t + 1_000, &enhanced());
        assert!(
            conns[0].stall_latched(),
            "a backlog pull still escalates once proof is one staleness window old"
        );
    }

    #[test]
    fn a_burst_on_a_quiet_link_waits_one_feedback_round_trip() {
        // A low-share 50 ms link lost its last keepalive echo (proof 1.6 s
        // old) and is handed a keyframe burst. No ACK for the burst can be
        // back yet, so it must not count until a round trip (and the margin)
        // has passed; after that, with still no proof, it does.
        use srtla_core::config_snapshot::{
            STALL_MIN_UNPROVEN_SENDS, STALL_UNPROVEN_FEEDBACK_RTT_MULT,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let t = now_ms();
        conns[0].rtt.update_estimate(50, t - 2_000);
        conns[0].last_ack_or_rtt_sample_ms = t - 1_600;
        send_drained(&mut conns[0], 100, STALL_MIN_UNPROVEN_SENDS, t);
        let select_at = |conns: &mut smallvec::SmallVec<_, 4>, at: u64| {
            make_healthy_busy(&mut conns[1], at);
            conns[0].last_received = Some(at);
            conns[1].last_received = Some(at);
            let _ = select_connection_idx(conns, None, at, &enhanced());
        };

        select_at(&mut conns, t + 1);
        assert!(
            !conns[0].stall_gated,
            "the burst cannot have been answered yet"
        );
        let rtt = conns[0].get_smooth_rtt_ms() as u64;
        select_at(&mut conns, t + rtt * STALL_UNPROVEN_FEEDBACK_RTT_MULT + 1);
        assert!(
            conns[0].stall_latched(),
            "unanswered after a round trip, it counts"
        );
    }

    #[test]
    fn a_short_stale_override_does_not_shorten_the_unproven_floor() {
        // `--stall-ack-stale-ms 750` caps the backlog window, not the
        // unproven-send floor.
        use srtla_core::config_snapshot::{
            STALL_MIN_UNPROVEN_SENDS, STALL_UNPROVEN_STALE_FLOOR_MS,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        conns[0].rtt.update_estimate(7, now - 2_000);
        send_drained(&mut conns[0], 100, STALL_MIN_UNPROVEN_SENDS, now - 1_400);
        make_healthy_busy(&mut conns[1], now);
        let short = ConfigSnapshot {
            stall_ack_stale_ms: 750,
            ..enhanced()
        };

        conns[0].last_ack_or_rtt_sample_ms = now - 1_000;
        let _ = select_connection_idx(&mut conns, None, now, &short);
        assert!(!conns[0].stall_gated);

        conns[0].last_ack_or_rtt_sample_ms = now - STALL_UNPROVEN_STALE_FLOOR_MS;
        let _ = select_connection_idx(&mut conns, None, now, &short);
        assert!(conns[0].stall_gated);
    }

    #[test]
    fn fewer_unproven_sends_than_the_threshold_do_not_stall() {
        use srtla_core::config_snapshot::STALL_MIN_UNPROVEN_SENDS;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        conns[0].last_ack_or_rtt_sample_ms = now.saturating_sub(STALL_ACK_STALE_MS + 1000);
        send_drained(
            &mut conns[0],
            100,
            STALL_MIN_UNPROVEN_SENDS - 1,
            now - 1_000,
        );
        make_healthy_busy(&mut conns[1], now);

        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(!conns[0].stall_gated);
        assert_eq!(conns[0].stall_gate_events(), 0);
    }

    #[test]
    fn any_delivery_proof_clears_the_unproven_count() {
        use srtla_core::config_snapshot::STALL_MIN_UNPROVEN_SENDS;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();

        // A keepalive echo.
        send_payload(&mut conns[0], 100, STALL_MIN_UNPROVEN_SENDS, now - 1_000);
        let ka = conns[0].keepalive_packet(now);
        conns[0].handle_keepalive_echo(&ka, now + 5);
        assert_eq!(conns[0].unproven_sends, 0);

        // An SRTLA ACK for a packet this link carried.
        send_payload(&mut conns[0], 200, STALL_MIN_UNPROVEN_SENDS, now);
        assert!(conns[0].handle_srtla_ack_specific(210, false, now + 5));
        assert_eq!(conns[0].unproven_sends, 0);

        // NAKs, cumulative ACKs and ACKs that came back on another link are
        // not proof and leave the count alone.
        send_payload(&mut conns[0], 300, STALL_MIN_UNPROVEN_SENDS, now);
        conns[0].handle_nak(300, false, now);
        conns[0].handle_srt_ack(305, now, true);
        assert!(conns[0].handle_srtla_ack_foreign(310));
        assert_eq!(conns[0].unproven_sends, STALL_MIN_UNPROVEN_SENDS);
    }

    #[test]
    fn probes_on_a_gated_link_are_not_unproven_payload() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();
        for seq in 0..100u32 {
            let mut pkt = [0u8; 16];
            pkt[..4].copy_from_slice(&seq.to_be_bytes());
            conns[0].queue_probe_packet(&pkt, seq, now);
        }
        let _ = conns[0].take_batch(now);
        assert_eq!(conns[0].unproven_sends, 0);
    }

    #[test]
    fn a_link_reset_forgets_its_unproven_sends() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        send_payload(&mut conns[0], 0, 50, now_ms());
        conns[0].mark_for_recovery();
        assert_eq!(conns[0].unproven_sends, 0);
    }

    #[test]
    fn registration_clears_unproven_sends() {
        // The shell can forward payload before REG3. Those packets owe no
        // proof once the link registers: a freshly warming link with nothing
        // in flight must not be pulled for them.
        use srtla_core::config_snapshot::STALL_MIN_UNPROVEN_SENDS;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let t = now_ms();
        send_payload(&mut conns[0], 100, STALL_MIN_UNPROVEN_SENDS, t - 1_000);
        conns[0].clear_pre_registration_state(t - 500);
        assert_eq!(conns[0].unproven_sends, 0);
        conns[0].last_received = Some(t - 251);
        make_healthy_busy(&mut conns[1], t);
        conns[1].last_received = Some(t);
        let _ = select_connection_idx(&mut conns, None, t, &enhanced());
        assert_eq!(conns[0].silence_pulls(), 0);
        assert!(!conns[0].stall_gated);
    }

    #[test]
    fn a_silent_link_with_unproven_payload_is_pulled() {
        // A full blackhole: nothing at all comes back, and the backlog has
        // been drained by NAKs arriving over the healthy link.
        use srtla_core::config_snapshot::STALL_MIN_UNPROVEN_SENDS;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        conns[0].last_ack_or_rtt_sample_ms = now;
        conns[0].last_received = Some(now - 300);
        send_payload(&mut conns[0], 100, STALL_MIN_UNPROVEN_SENDS, now - 1_000);
        for seq in 100..100 + STALL_MIN_UNPROVEN_SENDS as i32 {
            conns[0].handle_nak(seq, false, now);
        }
        make_healthy_busy(&mut conns[1], now);

        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(
            conns[0].stall_gated,
            "a mute link owed proof must be pulled"
        );
        assert_eq!(conns[0].silence_pulls(), 1);
    }

    // --- Runtime-scaled liveness timeout (librist c7a578d1's slow half) ---

    #[test]
    fn conn_timeout_is_runtime_scaled() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(1));
        let now = now_ms();

        conns[0].last_received = Some(now - 8_000);

        // Default 5 s window: 8 s of silence is a teardown.
        let _ = select_connection_idx(&mut conns, None, now, &enhanced());
        assert!(conns[0].is_timed_out(now));

        // A latency-aware client scaled the window to 12 s: the same silence
        // now rides through and the session resumes warm.
        let scaled = ConfigSnapshot {
            conn_timeout_ms: 12_000,
            ..enhanced()
        };
        let _ = select_connection_idx(&mut conns, None, now, &scaled);
        assert!(
            !conns[0].is_timed_out(now),
            "the scaled liveness window must reach is_timed_out callers"
        );
    }
}
