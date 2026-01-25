//! Runtime configuration for SRTLA sender.
//!
//! Manages dynamic settings that can be changed at runtime via stdin or Unix socket.

use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

#[cfg(unix)]
use tracing::debug;
use tracing::{info, warn};

use crate::mode::SchedulingMode;

/// Default RTT delta threshold in milliseconds.
/// Links within min_rtt + delta are considered "fast" and preferred.
pub const DEFAULT_RTT_DELTA_MS: u32 = 30;

/// Snapshot of configuration for efficient hot-path access.
/// Call `DynamicConfig::snapshot()` once per select iteration to avoid
/// multiple atomic loads per packet in the hot path.
#[derive(Clone, Copy, Debug)]
pub struct ConfigSnapshot {
    pub mode: SchedulingMode,
    pub quality_enabled: bool,
    pub exploration_enabled: bool,
    pub rtt_delta_ms: u32,
}

impl ConfigSnapshot {
    /// Check if quality scoring is effective for the current mode.
    /// Quality scoring only applies to enhanced and rtt-threshold modes.
    #[inline]
    pub fn effective_quality_enabled(&self) -> bool {
        self.quality_enabled && !self.mode.is_classic()
    }

    /// Check if exploration is effective for the current mode.
    /// Exploration only applies to enhanced mode.
    #[inline]
    pub fn effective_exploration_enabled(&self) -> bool {
        self.exploration_enabled && self.mode.is_enhanced()
    }
}

/// Dynamic configuration that can be modified at runtime.
/// Uses atomic types for lock-free concurrent access.
#[derive(Clone)]
pub struct DynamicConfig {
    mode: Arc<AtomicU8>,
    quality_enabled: Arc<AtomicBool>,
    exploration_enabled: Arc<AtomicBool>,
    rtt_delta_ms: Arc<AtomicU32>,
}

impl Default for DynamicConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicConfig {
    pub fn new() -> Self {
        Self {
            mode: Arc::new(AtomicU8::new(SchedulingMode::Enhanced.as_u8())),
            quality_enabled: Arc::new(AtomicBool::new(true)),
            exploration_enabled: Arc::new(AtomicBool::new(false)),
            rtt_delta_ms: Arc::new(AtomicU32::new(DEFAULT_RTT_DELTA_MS)),
        }
    }

    /// Create config from CLI arguments.
    pub fn from_cli(
        mode: SchedulingMode,
        no_quality: bool,
        exploration: bool,
        rtt_delta_ms: u32,
    ) -> Self {
        Self {
            mode: Arc::new(AtomicU8::new(mode.as_u8())),
            quality_enabled: Arc::new(AtomicBool::new(!no_quality)),
            exploration_enabled: Arc::new(AtomicBool::new(exploration)),
            rtt_delta_ms: Arc::new(AtomicU32::new(rtt_delta_ms)),
        }
    }

    /// Create a snapshot of current configuration.
    /// Call this once at the start of each select iteration to avoid
    /// multiple atomic loads per packet in the hot path.
    #[inline]
    pub fn snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            mode: SchedulingMode::from_u8(self.mode.load(Ordering::Relaxed)),
            quality_enabled: self.quality_enabled.load(Ordering::Relaxed),
            exploration_enabled: self.exploration_enabled.load(Ordering::Relaxed),
            rtt_delta_ms: self.rtt_delta_ms.load(Ordering::Relaxed),
        }
    }

    /// Get the current scheduling mode.
    #[inline]
    pub fn mode(&self) -> SchedulingMode {
        SchedulingMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Set the scheduling mode.
    pub fn set_mode(&self, mode: SchedulingMode) {
        self.mode.store(mode.as_u8(), Ordering::Relaxed);
    }

    /// Set whether quality scoring is enabled.
    pub fn set_quality_enabled(&self, enabled: bool) {
        self.quality_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Set whether exploration is enabled.
    pub fn set_exploration_enabled(&self, enabled: bool) {
        self.exploration_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Set the RTT delta threshold in milliseconds.
    pub fn set_rtt_delta_ms(&self, delta: u32) {
        self.rtt_delta_ms.store(delta, Ordering::Relaxed);
    }
}

pub fn spawn_config_listener(config: DynamicConfig, socket_path: Option<String>) {
    // Spawn stdin listener (backward compatibility)
    if socket_path.is_none() {
        let config_clone = config.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin);
            for cmd in reader.lines().map_while(Result::ok) {
                apply_cmd(&config_clone, cmd.trim());
            }
        });
    }

    // Spawn Unix domain socket listener if socket path specified
    #[cfg(unix)]
    if let Some(sock_path) = socket_path {
        let config_clone = config.clone();
        std::thread::spawn(move || {
            unix_socket_loop(&config_clone, &sock_path);
        });
    }
}

/// Apply a runtime command to the configuration.
///
/// Commands:
/// - `mode classic|enhanced|rtt-threshold` - switch scheduling mode
/// - `quality on|off` - toggle quality scoring
/// - `explore on|off` - toggle exploration
/// - `rtt-delta <ms>` - set RTT delta threshold
/// - `status` - show current configuration
pub fn apply_cmd(config: &DynamicConfig, cmd: &str) {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return;
    }

    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    match parts[0] {
        "mode" => {
            if parts.len() != 2 {
                warn!("usage: mode classic|enhanced|rtt-threshold");
                return;
            }
            match parts[1] {
                "classic" => {
                    config.set_mode(SchedulingMode::Classic);
                    info!("mode: classic");
                }
                "enhanced" => {
                    config.set_mode(SchedulingMode::Enhanced);
                    info!("mode: enhanced");
                }
                "rtt-threshold" => {
                    config.set_mode(SchedulingMode::RttThreshold);
                    info!("mode: rtt-threshold");
                }
                other => {
                    warn!(
                        "unknown mode '{}': use classic, enhanced, or rtt-threshold",
                        other
                    );
                }
            }
        }

        "quality" => {
            if parts.len() != 2 {
                warn!("usage: quality on|off");
                return;
            }
            match parts[1] {
                "on" => {
                    config.set_quality_enabled(true);
                    info!("quality: on");
                }
                "off" => {
                    config.set_quality_enabled(false);
                    info!("quality: off");
                }
                other => {
                    warn!("invalid value '{}': use on or off", other);
                }
            }
        }

        "explore" => {
            if parts.len() != 2 {
                warn!("usage: explore on|off");
                return;
            }
            match parts[1] {
                "on" => {
                    config.set_exploration_enabled(true);
                    info!("explore: on");
                }
                "off" => {
                    config.set_exploration_enabled(false);
                    info!("explore: off");
                }
                other => {
                    warn!("invalid value '{}': use on or off", other);
                }
            }
        }

        "rtt-delta" => {
            if parts.len() != 2 {
                warn!("usage: rtt-delta <ms>");
                return;
            }
            match parts[1].parse::<u32>() {
                Ok(delta) => {
                    config.set_rtt_delta_ms(delta);
                    info!("rtt-delta: {}ms", delta);
                }
                Err(_) => {
                    warn!("invalid rtt-delta value: {}", parts[1]);
                }
            }
        }

        "status" => {
            let snap = config.snapshot();
            info!("mode: {}", snap.mode);
            info!(
                "  quality: {}",
                if snap.quality_enabled { "on" } else { "off" }
            );
            info!(
                "  explore: {}",
                if snap.exploration_enabled {
                    "on"
                } else {
                    "off"
                }
            );
            info!("  rtt-delta: {}ms", snap.rtt_delta_ms);
        }

        other => {
            warn!("unknown command: {}", other);
        }
    }
}

#[cfg(unix)]
fn unix_socket_loop(config: &DynamicConfig, socket_path: &str) {
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            warn!("failed to bind unix socket {}: {}", socket_path, e);
            return;
        }
    };

    info!("unix socket listening at: {}", socket_path);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let config_clone = config.clone();
                std::thread::spawn(move || {
                    handle_unix_client(config_clone, stream);
                });
            }
            Err(e) => {
                debug!("unix socket accept error: {}", e);
            }
        }
    }
}

#[cfg(unix)]
fn handle_unix_client(config: DynamicConfig, stream: UnixStream) {
    let reader = BufReader::new(&stream);
    for line in reader.lines() {
        match line {
            Ok(cmd) => {
                apply_cmd(&config, cmd.trim());
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = DynamicConfig::new();
        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Enhanced);
        assert!(snap.quality_enabled);
        assert!(!snap.exploration_enabled);
        assert_eq!(snap.rtt_delta_ms, DEFAULT_RTT_DELTA_MS);
    }

    #[test]
    fn test_config_from_cli() {
        let config = DynamicConfig::from_cli(SchedulingMode::Classic, true, true, 50);
        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Classic);
        assert!(!snap.quality_enabled); // no_quality=true means disabled
        assert!(snap.exploration_enabled);
        assert_eq!(snap.rtt_delta_ms, 50);
    }

    #[test]
    fn test_mode_commands() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "mode classic");
        assert_eq!(config.mode(), SchedulingMode::Classic);

        apply_cmd(&config, "mode enhanced");
        assert_eq!(config.mode(), SchedulingMode::Enhanced);

        apply_cmd(&config, "mode rtt-threshold");
        assert_eq!(config.mode(), SchedulingMode::RttThreshold);
    }

    #[test]
    fn test_quality_commands() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "quality off");
        assert!(!config.snapshot().quality_enabled);

        apply_cmd(&config, "quality on");
        assert!(config.snapshot().quality_enabled);
    }

    #[test]
    fn test_exploration_commands() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "explore on");
        assert!(config.snapshot().exploration_enabled);

        apply_cmd(&config, "explore off");
        assert!(!config.snapshot().exploration_enabled);
    }

    #[test]
    fn test_rtt_delta_commands() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "rtt-delta 50");
        assert_eq!(config.snapshot().rtt_delta_ms, 50);

        apply_cmd(&config, "rtt-delta 100");
        assert_eq!(config.snapshot().rtt_delta_ms, 100);
    }

    #[test]
    fn test_effective_quality() {
        // Classic mode - quality never effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: true,
            exploration_enabled: true,
            rtt_delta_ms: 30,
        };
        assert!(!snap.effective_quality_enabled());
        assert!(!snap.effective_exploration_enabled());

        // Enhanced mode - both can be effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: true,
            rtt_delta_ms: 30,
        };
        assert!(snap.effective_quality_enabled());
        assert!(snap.effective_exploration_enabled());

        // RTT-threshold mode - quality effective, exploration not
        let snap = ConfigSnapshot {
            mode: SchedulingMode::RttThreshold,
            quality_enabled: true,
            exploration_enabled: true,
            rtt_delta_ms: 30,
        };
        assert!(snap.effective_quality_enabled());
        assert!(!snap.effective_exploration_enabled());
    }

    #[test]
    fn test_concurrent_access() {
        use std::thread;

        let config = DynamicConfig::new();
        let config_clone = config.clone();

        let writer = thread::spawn(move || {
            for _ in 0..100 {
                config_clone.set_mode(SchedulingMode::Classic);
                config_clone.set_mode(SchedulingMode::Enhanced);
                config_clone.set_mode(SchedulingMode::RttThreshold);
            }
        });

        let config_clone2 = config.clone();
        let reader = thread::spawn(move || {
            for _ in 0..100 {
                let _ = config_clone2.snapshot();
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }
}
