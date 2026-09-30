//! Tests for operator link weights (Moblin's "connection priorities"): the
//! optional second column of the IPs file, normalised so the lowest link is 1,
//! applied by classic selection only through a three-band window multiplier.
//! Parsing and the reload guard are covered in `sender::reload`'s own tests.

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use srtla_core::connection::{
        LINK_WEIGHT_FULL_ABOVE_WINDOW, LINK_WEIGHT_NONE_AT_OR_BELOW_WINDOW, link_weight_multiplier,
        normalise_link_weights,
    };
    use srtla_core::mode::SchedulingMode;
    use srtla_core::selection::select_connection_idx;
    use srtla_core::utils::now_ms;

    use crate::config::{ConfigSnapshot, STALL_ACK_STALE_MS, STALL_MIN_IN_FLIGHT_PACKETS};
    use crate::sender::apply_link_weights;
    use crate::stats::SharedStats;
    use crate::test_helpers::create_test_connections;

    fn classic() -> ConfigSnapshot {
        ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: false,
            ..ConfigSnapshot::default()
        }
    }

    /// Feed `packets` packets through classic selection, counting each pick as
    /// one more in-flight packet on the chosen link (no ACKs): the share each
    /// link ends up with is the scheduler's steady preference at these windows.
    fn picks(conns: &mut [srtla_core::connection::SrtlaConnection], packets: usize) -> Vec<usize> {
        let config = classic();
        let mut counts = vec![0usize; conns.len()];
        for _ in 0..packets {
            let now = now_ms();
            let idx = select_connection_idx(conns, None, now, &config).expect("a link");
            conns[idx].in_flight_packets += 1;
            counts[idx] += 1;
        }
        counts
    }

    #[test]
    fn multiplier_full_weight_above_20k() {
        assert_eq!(link_weight_multiplier(60_000, 10), 10.0);
        assert_eq!(link_weight_multiplier(20_001, 10), 10.0);
        assert_eq!(
            link_weight_multiplier(LINK_WEIGHT_FULL_ABOVE_WINDOW, 10),
            10.0
        );
    }

    #[test]
    fn multiplier_fades_linearly_between_10k_and_20k() {
        assert_eq!(link_weight_multiplier(15_000, 10), 5.5);
        assert_eq!(link_weight_multiplier(12_500, 5), 2.0);
        let just_above = link_weight_multiplier(LINK_WEIGHT_NONE_AT_OR_BELOW_WINDOW + 1, 10);
        assert!(just_above > 1.0 && just_above < 1.01, "{just_above}");
    }

    #[test]
    fn multiplier_ignores_weight_at_or_below_10k() {
        assert_eq!(
            link_weight_multiplier(LINK_WEIGHT_NONE_AT_OR_BELOW_WINDOW, 10),
            1.0
        );
        assert_eq!(link_weight_multiplier(1_000, 10), 1.0);
    }

    #[test]
    fn weight_one_is_neutral_in_every_band() {
        for window in [60_000, 20_000, 15_000, 10_000, 1] {
            assert_eq!(link_weight_multiplier(window, 1), 1.0);
        }
    }

    #[test]
    fn normalisation_makes_the_lowest_weight_one() {
        let mut w = [10u8, 1];
        normalise_link_weights(&mut w);
        assert_eq!(w, [10, 1]);
        let mut w = [10u8, 5, 7];
        normalise_link_weights(&mut w);
        assert_eq!(w, [6, 1, 3]);
        let mut w = [4u8, 4];
        normalise_link_weights(&mut w);
        assert_eq!(w, [1, 1]);
        let mut w = [0u8, 200];
        normalise_link_weights(&mut w);
        assert_eq!(w, [1, 10], "clamped to 1..10 before normalising");
        let mut w: [u8; 0] = [];
        normalise_link_weights(&mut w);
    }

    #[test]
    fn unweighted_links_share_evenly() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let counts = picks(&mut conns, 200);
        assert_eq!(counts, vec![100, 100]);
    }

    #[test]
    fn classic_prefers_the_weighted_link_while_healthy() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        for c in conns.iter_mut() {
            c.window = 40_000; // healthy band
        }
        conns[0].link_weight = 10;
        let counts = picks(&mut conns, 220);
        let share_b = counts[1] as f64 / 220.0;
        assert!(counts[0] > counts[1], "{counts:?}");
        assert!(
            share_b > 0.0 && share_b < 0.12,
            "weight 10 leaves roughly one packet in ten to the other link: {counts:?}"
        );
    }

    #[test]
    fn preference_fades_as_the_weighted_links_window_shrinks() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let share_b_at = |window_a: i32| {
            let mut conns = rt.block_on(create_test_connections(2));
            conns[0].window = window_a;
            conns[1].window = window_a;
            conns[0].link_weight = 10;
            let counts = picks(&mut conns, 400);
            counts[1] as f64 / 400.0
        };
        let healthy = share_b_at(40_000);
        let shedding = share_b_at(15_000);
        let congested = share_b_at(10_000);
        assert!(healthy < shedding, "{healthy} {shedding}");
        assert!(shedding < congested, "{shedding} {congested}");
        assert!(
            (congested - 0.5).abs() < 0.01,
            "at or below 10k the weight is ignored: {congested}"
        );
    }

    #[test]
    fn a_small_window_on_the_weighted_link_hands_traffic_to_the_other() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        conns[0].link_weight = 10;
        conns[0].window = 5_000; // A is congested: no multiplier
        conns[1].window = 40_000; // B healthy, weight 1
        let counts = picks(&mut conns, 90);
        assert!(counts[1] > counts[0] * 4, "{counts:?}");
    }

    #[test]
    fn timed_out_weighted_link_is_still_skipped() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        conns[0].link_weight = 10;
        conns[0].window = 60_000;
        conns[0].last_received = Some(now.saturating_sub(60_000));
        conns[1].in_flight_packets = 30;
        let selected = select_connection_idx(&mut conns, None, now, &classic());
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn stall_gated_weighted_link_is_still_skipped() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let now = now_ms();
        conns[0].link_weight = 10;
        conns[0].in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS;
        conns[0].last_ack_or_rtt_sample_ms = now.saturating_sub(STALL_ACK_STALE_MS + 1000);
        conns[1].in_flight_packets = STALL_MIN_IN_FLIGHT_PACKETS * 2;
        conns[1].last_ack_or_rtt_sample_ms = now;
        let selected = select_connection_idx(&mut conns, None, now, &classic());
        assert_eq!(selected, Some(1));
        assert!(conns[0].stall_gated);
    }

    #[test]
    fn enhanced_mode_ignores_weights() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        conns[0].link_weight = 10;
        conns[0].in_flight_packets = 5;
        conns[1].in_flight_packets = 0;
        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            ..ConfigSnapshot::default()
        };
        let selected = select_connection_idx(&mut conns, None, now_ms(), &config);
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn reload_reweights_existing_links_and_unnamed_links_fall_back_to_one() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        let a = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        let b = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 11));
        assert_eq!(conns[0].local_ip, a);

        apply_link_weights(&mut conns, &[a, b], &[10, 1]);
        assert_eq!((conns[0].link_weight, conns[1].link_weight), (10, 1));

        // A reload without weights puts everyone back to 1.
        apply_link_weights(&mut conns, &[a, b], &[1, 1]);
        assert_eq!((conns[0].link_weight, conns[1].link_weight), (1, 1));

        // Weight moves to the other link.
        apply_link_weights(&mut conns, &[a, b], &[1, 7]);
        assert_eq!((conns[0].link_weight, conns[1].link_weight), (1, 7));

        // An empty weight list (legacy caller) means 1.
        apply_link_weights(&mut conns, &[a, b], &[]);
        assert_eq!((conns[0].link_weight, conns[1].link_weight), (1, 1));
    }

    #[test]
    fn weight_is_exported_per_link_in_metrics() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));
        conns[0].link_weight = 10;
        let stats = SharedStats::new();
        stats.update(&conns, &classic(), None, None);
        let text = crate::metrics::render(
            &stats,
            &crate::config::DynamicConfig::new(),
            &srtla_core::priority::CriticalWindow::new(),
        );
        assert!(
            text.contains(r#"srtla_send_link_weight{ip="192.168.1.10"} 10"#),
            "{text}"
        );
        assert!(text.contains(r#"srtla_send_link_weight{ip="192.168.1.11"} 1"#));
    }
}
