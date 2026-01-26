#[cfg(test)]
mod tests {
    use crate::config::{DynamicConfig, apply_cmd};
    use crate::mode::SchedulingMode;

    #[test]
    fn test_config_new() {
        let config = DynamicConfig::new();
        let snap = config.snapshot();

        assert_eq!(snap.mode, SchedulingMode::Enhanced);
        assert!(snap.quality_enabled);
        assert!(!snap.exploration_enabled);
        assert_eq!(snap.rtt_delta_ms, 30);
    }

    #[test]
    fn test_config_from_cli() {
        let config = DynamicConfig::from_cli(SchedulingMode::Enhanced, false, false, 30);
        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Enhanced);
        assert!(snap.quality_enabled);
        assert!(!snap.exploration_enabled);

        let config = DynamicConfig::from_cli(SchedulingMode::Classic, true, true, 50);
        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Classic);
        assert!(!snap.quality_enabled);
        assert!(snap.exploration_enabled);
        assert_eq!(snap.rtt_delta_ms, 50);
    }

    #[test]
    fn test_apply_cmd_mode() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "mode classic");
        assert_eq!(config.mode(), SchedulingMode::Classic);

        apply_cmd(&config, "mode enhanced");
        assert_eq!(config.mode(), SchedulingMode::Enhanced);

        apply_cmd(&config, "mode rtt-threshold");
        assert_eq!(config.mode(), SchedulingMode::RttThreshold);
    }

    #[test]
    fn test_apply_cmd_quality() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "quality off");
        assert!(!config.snapshot().quality_enabled);

        apply_cmd(&config, "quality on");
        assert!(config.snapshot().quality_enabled);
    }

    #[test]
    fn test_apply_cmd_explore() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "explore on");
        assert!(config.snapshot().exploration_enabled);

        apply_cmd(&config, "explore off");
        assert!(!config.snapshot().exploration_enabled);
    }

    #[test]
    fn test_apply_cmd_rtt_delta() {
        let config = DynamicConfig::new();
        assert_eq!(config.snapshot().rtt_delta_ms, 30);

        apply_cmd(&config, "rtt-delta 50");
        assert_eq!(config.snapshot().rtt_delta_ms, 50);

        apply_cmd(&config, "rtt-delta 100");
        assert_eq!(config.snapshot().rtt_delta_ms, 100);

        // invalid value should not change
        apply_cmd(&config, "rtt-delta invalid");
        assert_eq!(config.snapshot().rtt_delta_ms, 100);
    }

    #[test]
    fn test_apply_cmd_status() {
        let config = DynamicConfig::new();

        let snap_before = config.snapshot();

        apply_cmd(&config, "status");

        let snap_after = config.snapshot();
        assert_eq!(snap_after.mode, snap_before.mode);
        assert_eq!(snap_after.quality_enabled, snap_before.quality_enabled);
        assert_eq!(
            snap_after.exploration_enabled,
            snap_before.exploration_enabled
        );
    }

    #[test]
    fn test_apply_cmd_empty_and_unknown() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "");
        apply_cmd(&config, "   ");
        apply_cmd(&config, "unknown command");

        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Enhanced);
        assert!(snap.quality_enabled);
        assert!(!snap.exploration_enabled);
    }

    #[test]
    fn test_apply_cmd_whitespace_handling() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "  mode classic  ");
        assert_eq!(config.mode(), SchedulingMode::Classic);
    }

    #[test]
    fn test_config_concurrent_access() {
        use std::thread;
        use std::time::Duration;

        let config = DynamicConfig::new();
        let config_clone = config.clone();

        let handle = thread::spawn(move || {
            for _ in 0..100 {
                apply_cmd(&config_clone, "mode classic");
                apply_cmd(&config_clone, "mode enhanced");
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
            exploration_enabled: true,
            rtt_delta_ms: 30,
        };
        assert!(!snap.effective_quality_enabled());
        assert!(!snap.effective_exploration_enabled());

        // enhanced mode - both can be effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: true,
            rtt_delta_ms: 30,
        };
        assert!(snap.effective_quality_enabled());
        assert!(snap.effective_exploration_enabled());

        // rtt-threshold mode - quality effective, exploration not
        let snap = ConfigSnapshot {
            mode: SchedulingMode::RttThreshold,
            quality_enabled: true,
            exploration_enabled: true,
            rtt_delta_ms: 30,
        };
        assert!(snap.effective_quality_enabled());
        assert!(!snap.effective_exploration_enabled());
    }
}
