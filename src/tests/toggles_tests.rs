#[cfg(test)]
mod tests {

    use std::sync::atomic::Ordering;

    use crate::toggles::*;

    #[test]
    fn test_dynamic_toggles_new() {
        let toggles = DynamicToggles::new();

        assert!(!toggles.classic_mode.load(Ordering::Relaxed));
        assert!(toggles.quality_scoring_enabled.load(Ordering::Relaxed));
        assert!(!toggles.exploration_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_dynamic_toggles_from_cli() {
        let toggles = DynamicToggles::from_cli(false, false, false);
        assert!(!toggles.classic_mode.load(Ordering::Relaxed));
        assert!(toggles.quality_scoring_enabled.load(Ordering::Relaxed));
        assert!(!toggles.exploration_enabled.load(Ordering::Relaxed));

        let toggles = DynamicToggles::from_cli(true, true, true);
        assert!(toggles.classic_mode.load(Ordering::Relaxed));
        assert!(!toggles.quality_scoring_enabled.load(Ordering::Relaxed));
        assert!(toggles.exploration_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_apply_cmd_classic() {
        let toggles = DynamicToggles::new();

        crate::toggles::apply_cmd(&toggles, "classic on");
        assert!(toggles.classic_mode.load(Ordering::Relaxed));

        crate::toggles::apply_cmd(&toggles, "classic off");
        assert!(!toggles.classic_mode.load(Ordering::Relaxed));

        crate::toggles::apply_cmd(&toggles, "classic=true");
        assert!(toggles.classic_mode.load(Ordering::Relaxed));

        crate::toggles::apply_cmd(&toggles, "classic=false");
        assert!(!toggles.classic_mode.load(Ordering::Relaxed));
    }

    #[test]
    fn test_apply_cmd_quality() {
        let toggles = DynamicToggles::new();

        crate::toggles::apply_cmd(&toggles, "quality off");
        assert!(!toggles.quality_scoring_enabled.load(Ordering::Relaxed));

        crate::toggles::apply_cmd(&toggles, "quality on");
        assert!(toggles.quality_scoring_enabled.load(Ordering::Relaxed));

        // Test alternative format
        crate::toggles::apply_cmd(&toggles, "quality=false");
        assert!(!toggles.quality_scoring_enabled.load(Ordering::Relaxed));

        crate::toggles::apply_cmd(&toggles, "quality=true");
        assert!(toggles.quality_scoring_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_apply_cmd_exploration() {
        let toggles = DynamicToggles::new();

        crate::toggles::apply_cmd(&toggles, "explore on");
        assert!(toggles.exploration_enabled.load(Ordering::Relaxed));

        crate::toggles::apply_cmd(&toggles, "explore off");
        assert!(!toggles.exploration_enabled.load(Ordering::Relaxed));

        // Test alternative format
        crate::toggles::apply_cmd(&toggles, "exploration=true");
        assert!(toggles.exploration_enabled.load(Ordering::Relaxed));

        crate::toggles::apply_cmd(&toggles, "exploration=false");
        assert!(!toggles.exploration_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_apply_cmd_status() {
        let toggles = DynamicToggles::new();

        let classic_before = toggles.classic_mode.load(Ordering::Relaxed);
        let quality_before = toggles.quality_scoring_enabled.load(Ordering::Relaxed);
        let explore_before = toggles.exploration_enabled.load(Ordering::Relaxed);

        crate::toggles::apply_cmd(&toggles, "status");

        assert_eq!(toggles.classic_mode.load(Ordering::Relaxed), classic_before);
        assert_eq!(
            toggles.quality_scoring_enabled.load(Ordering::Relaxed),
            quality_before
        );
        assert_eq!(
            toggles.exploration_enabled.load(Ordering::Relaxed),
            explore_before
        );
    }

    #[test]
    fn test_apply_cmd_empty_and_unknown() {
        let toggles = DynamicToggles::new();

        crate::toggles::apply_cmd(&toggles, "");
        crate::toggles::apply_cmd(&toggles, "   ");

        crate::toggles::apply_cmd(&toggles, "unknown command");
        crate::toggles::apply_cmd(&toggles, "invalid=value");

        assert!(!toggles.classic_mode.load(Ordering::Relaxed));
        assert!(toggles.quality_scoring_enabled.load(Ordering::Relaxed));
        assert!(!toggles.exploration_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_apply_cmd_whitespace_handling() {
        let toggles = DynamicToggles::new();

        crate::toggles::apply_cmd(&toggles, "  classic on  ");
        assert!(toggles.classic_mode.load(Ordering::Relaxed));
    }

    #[test]
    fn test_toggles_concurrent_access() {
        use std::thread;
        use std::time::Duration;

        let toggles = DynamicToggles::new();
        let toggles_clone = toggles.clone();

        // Spawn a thread that toggles values
        let handle = thread::spawn(move || {
            for _ in 0..100 {
                crate::toggles::apply_cmd(&toggles_clone, "classic on");
                crate::toggles::apply_cmd(&toggles_clone, "classic off");
            }
        });

        // Read values from main thread
        for _ in 0..100 {
            let _ = toggles.classic_mode.load(Ordering::Relaxed);
            thread::sleep(Duration::from_millis(1));
        }

        handle.join().unwrap();
    }

    #[test]
    fn test_spawn_toggle_listener_no_socket() {
        let toggles = DynamicToggles::new();

        // This should spawn a stdin listener without panicking
        spawn_toggle_listener(toggles, None);

        // Give it a moment to start up
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    #[cfg(unix)]
    #[test]
    fn test_spawn_toggle_listener_with_socket() {
        use tempfile::NamedTempFile;

        let toggles = DynamicToggles::new();
        let temp_file = NamedTempFile::new().unwrap();
        let socket_path = temp_file.path().to_string_lossy().to_string();

        // Remove the temp file so the socket can use the path
        drop(temp_file);

        // This should spawn both stdin and Unix socket listeners
        spawn_toggle_listener(toggles, Some(socket_path));

        // Give it a moment to start up
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    #[test]
    fn test_all_command_formats() {
        let toggles = DynamicToggles::new();

        // Test all supported command formats
        let test_commands = vec![
            ("classic on", "classic off"),
            ("classic=true", "classic=false"),
            ("quality on", "quality off"),
            ("quality=true", "quality=false"),
            ("explore on", "explore off"),
            ("exploration=true", "exploration=false"),
        ];

        for (on_cmd, off_cmd) in test_commands {
            crate::toggles::apply_cmd(&toggles, "classic=false");
            crate::toggles::apply_cmd(&toggles, "quality=false");
            crate::toggles::apply_cmd(&toggles, "exploration=false");

            let initial_classic = toggles.classic_mode.load(Ordering::Relaxed);
            let initial_quality = toggles.quality_scoring_enabled.load(Ordering::Relaxed);
            let initial_explore = toggles.exploration_enabled.load(Ordering::Relaxed);

            crate::toggles::apply_cmd(&toggles, on_cmd);

            let after_on = (
                toggles.classic_mode.load(Ordering::Relaxed),
                toggles.quality_scoring_enabled.load(Ordering::Relaxed),
                toggles.exploration_enabled.load(Ordering::Relaxed),
            );

            let initial = (initial_classic, initial_quality, initial_explore);
            assert_ne!(
                after_on, initial,
                "Command '{}' should change state",
                on_cmd
            );

            crate::toggles::apply_cmd(&toggles, off_cmd);

            let after_off = (
                toggles.classic_mode.load(Ordering::Relaxed),
                toggles.quality_scoring_enabled.load(Ordering::Relaxed),
                toggles.exploration_enabled.load(Ordering::Relaxed),
            );

            assert_ne!(
                after_off, after_on,
                "Command '{}' should change state",
                off_cmd
            );
        }
    }
}
