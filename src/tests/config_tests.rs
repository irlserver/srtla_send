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
