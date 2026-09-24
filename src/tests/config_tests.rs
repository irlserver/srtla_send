#[cfg(test)]
mod tests {
    use srtla_core::mode::SchedulingMode;

    use crate::config::DynamicConfig;

    #[test]
    fn test_config_new() {
        let config = DynamicConfig::new();
        let snap = config.snapshot();

        assert_eq!(snap.mode, SchedulingMode::Enhanced);
        assert!(snap.quality_enabled);
    }

    #[test]
    fn test_config_from_cli() {
        let config = DynamicConfig::from_cli(
            SchedulingMode::Enhanced,
            false,
            false,
            crate::config::STALL_MIN_IN_FLIGHT_PACKETS,
            crate::config::STALL_ACK_STALE_MS,
            crate::config::CONN_TIMEOUT_MS,
            crate::config::RECONNECT_FAST_RETRY_MS,
            crate::config::RECONNECT_FAST_RETRY_ATTEMPTS,
            false,
        );
        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Enhanced);
        assert!(snap.quality_enabled);
        assert!(snap.stall_deselect);

        let config = DynamicConfig::from_cli(
            SchedulingMode::Classic,
            true,
            true,
            crate::config::STALL_MIN_IN_FLIGHT_PACKETS,
            crate::config::STALL_ACK_STALE_MS,
            crate::config::CONN_TIMEOUT_MS,
            crate::config::RECONNECT_FAST_RETRY_MS,
            crate::config::RECONNECT_FAST_RETRY_ATTEMPTS,
            false,
        );
        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Classic);
        assert!(!snap.quality_enabled);
        assert!(!snap.stall_deselect); // no_stall_deselect=true disables it
    }

    #[test]
    fn test_conn_timeout_clamped() {
        let config = DynamicConfig::new();
        assert_eq!(config.set_conn_timeout_ms(100), 1_000, "floor");
        assert_eq!(config.set_conn_timeout_ms(120_000), 60_000, "ceiling");
        assert_eq!(config.set_conn_timeout_ms(9_000), 9_000);
        assert_eq!(config.snapshot().conn_timeout_ms, 9_000);
    }

    #[test]
    fn test_reconnect_fast_retry_defaults_and_clamps() {
        let config = DynamicConfig::new();
        let snap = config.snapshot();
        assert_eq!(snap.reconnect_fast_retry_ms, 1_000);
        assert_eq!(snap.reconnect_fast_retry_attempts, 4);

        assert_eq!(
            config.set_reconnect_fast_retry(Some(100), None),
            (1_000, 4),
            "floor"
        );
        assert_eq!(
            config.set_reconnect_fast_retry(Some(60_000), None),
            (5_000, 4),
            "ceiling"
        );
        assert_eq!(
            config.set_reconnect_fast_retry(None, Some(99)),
            (5_000, 10),
            "attempts cap"
        );
        assert_eq!(
            config.set_reconnect_fast_retry(Some(2_000), Some(0)),
            (2_000, 0),
            "opt-out"
        );
        let snap = config.snapshot();
        assert_eq!(snap.reconnect_fast_retry_ms, 2_000);
        assert_eq!(snap.reconnect_fast_retry_attempts, 0);

        let from_cli = DynamicConfig::from_cli(
            SchedulingMode::Enhanced,
            false,
            false,
            crate::config::STALL_MIN_IN_FLIGHT_PACKETS,
            crate::config::STALL_ACK_STALE_MS,
            crate::config::CONN_TIMEOUT_MS,
            200,
            50,
            false,
        );
        let snap = from_cli.snapshot();
        assert_eq!(
            (
                snap.reconnect_fast_retry_ms,
                snap.reconnect_fast_retry_attempts
            ),
            (1_000, 10),
            "CLI values are clamped like conn_timeout_ms"
        );
    }

    #[test]
    fn test_set_reconnect_fast_retry_over_control() {
        let config = DynamicConfig::new();
        let resp = crate::control::dispatch(
            &config,
            None,
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"set_reconnect_fast_retry","params":{"attempts":0}}"#,
        )
        .unwrap()
        .to_json();
        assert!(resp.contains(r#""attempts":0"#), "{resp}");
        assert!(resp.contains(r#""ms":1000"#), "{resp}");
        assert_eq!(config.snapshot().reconnect_fast_retry_attempts, 0);

        let bad = crate::control::dispatch(
            &config,
            None,
            None,
            r#"{"jsonrpc":"2.0","id":2,"method":"set_reconnect_fast_retry","params":{}}"#,
        )
        .unwrap()
        .to_json();
        assert!(bad.contains("-32602"), "{bad}");
    }

    #[test]
    fn test_config_concurrent_access() {
        use std::thread;
        use std::time::Duration;

        let config = DynamicConfig::new();
        let config_clone = config.clone();

        let handle = thread::spawn(move || {
            for _ in 0..100 {
                config_clone.set_mode(SchedulingMode::Classic);
                config_clone.set_mode(SchedulingMode::Enhanced);
            }
        });

        for _ in 0..100 {
            let _ = config.snapshot();
            thread::sleep(Duration::from_millis(1));
        }

        handle.join().unwrap();
    }

    #[test]
    fn test_effective_quality_enabled() {
        use crate::config::ConfigSnapshot;

        // classic mode - quality never effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: true,
            ..ConfigSnapshot::default()
        };
        assert!(!snap.effective_quality_enabled());

        // enhanced mode - both can be effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            ..ConfigSnapshot::default()
        };
        assert!(snap.effective_quality_enabled());
    }
}
