//! Runtime configuration for SRTLA sender.
//!
//! Manages dynamic settings that can be changed at runtime via stdin or Unix socket.

#[cfg(unix)]
use std::io::Write;
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

#[cfg(unix)]
use tracing::debug;
use tracing::{info, warn};

use crate::mode::SchedulingMode;
use crate::stats::SharedStats;

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
    /// Packets still to be treated as critical, supplied out-of-band by an
    /// encoder that knows which frames are IDR/SPS/PPS. Decremented by one
    /// per forwarded SRT data packet. Augments (does not replace) the
    /// packet-size keyframe heuristic in [`crate::sender::keyframe`].
    critical_hint_remaining: Arc<AtomicU32>,
    /// Monotonic count of `mark-critical` commands received. Exposed for
    /// telemetry so it's obvious whether the hint channel is live.
    critical_hints_total: Arc<AtomicU32>,
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
            critical_hint_remaining: Arc::new(AtomicU32::new(0)),
            critical_hints_total: Arc::new(AtomicU32::new(0)),
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
            critical_hint_remaining: Arc::new(AtomicU32::new(0)),
            critical_hints_total: Arc::new(AtomicU32::new(0)),
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

    /// Add `count` packets to the critical-hint budget. Called from the
    /// control socket when an upstream encoder signals that the next N SRT
    /// data packets carry IDR / parameter-set / other must-land bytes.
    pub fn add_critical_hint(&self, count: u32) {
        if count == 0 {
            return;
        }
        self.critical_hint_remaining
            .fetch_add(count, Ordering::Relaxed);
        self.critical_hints_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Consume one packet from the critical-hint budget. Returns `true` if
    /// the packet should be scheduled as critical. Cheap enough for the
    /// per-packet hot path (one atomic CAS on the fast path).
    #[inline]
    pub fn consume_critical_hint(&self) -> bool {
        let mut cur = self.critical_hint_remaining.load(Ordering::Relaxed);
        while cur > 0 {
            match self.critical_hint_remaining.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
        false
    }

    /// Non-consuming peek, for telemetry.
    pub fn critical_hint_remaining(&self) -> u32 {
        self.critical_hint_remaining.load(Ordering::Relaxed)
    }

    /// Total hints received since start, for telemetry.
    pub fn critical_hints_total(&self) -> u32 {
        self.critical_hints_total.load(Ordering::Relaxed)
    }
}

pub fn spawn_config_listener(
    config: DynamicConfig,
    socket_path: Option<String>,
    stats: SharedStats,
) {
    if let Some(sock_path) = socket_path {
        // Socket path specified: use Unix socket on Unix, fallback to stdin on other platforms
        #[cfg(unix)]
        {
            let config_clone = config.clone();
            let stats_clone = stats.clone();
            std::thread::spawn(move || {
                unix_socket_loop(&config_clone, &sock_path, &stats_clone);
            });
        }
        #[cfg(not(unix))]
        {
            // Unix sockets not available; fall back to stdin listener
            let _ = sock_path; // suppress unused warning
            let _ = stats; // suppress unused warning
            let config_clone = config.clone();
            std::thread::spawn(move || {
                let stdin = std::io::stdin();
                let reader = BufReader::new(stdin);
                for cmd in reader.lines().map_while(Result::ok) {
                    apply_cmd(&config_clone, cmd.trim(), None);
                }
            });
        }
    } else {
        // No socket path: use stdin listener (backward compatibility)
        let config_clone = config.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin);
            for cmd in reader.lines().map_while(Result::ok) {
                apply_cmd(&config_clone, cmd.trim(), None);
            }
        });
    }
}

/// Response from apply_cmd that can be sent back to the client.
#[allow(dead_code)] // Json variant's inner value is read in #[cfg(unix)] code
pub enum CmdResponse {
    /// No response needed (command logged via tracing)
    None,
    /// JSON response to send back
    Json(String),
}

/// Apply a runtime command to the configuration.
///
/// Commands:
/// - `mode classic|enhanced|rtt-threshold` - switch scheduling mode
/// - `quality on|off` - toggle quality scoring
/// - `explore on|off` - toggle exploration
/// - `rtt-delta <ms>` - set RTT delta threshold
/// - `status` - show current configuration
/// - `stats` - get per-link telemetry as JSON
pub fn apply_cmd(config: &DynamicConfig, cmd: &str, stats: Option<&SharedStats>) -> CmdResponse {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return CmdResponse::None;
    }

    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return CmdResponse::None;
    }

    match parts[0] {
        "mode" => {
            if parts.len() != 2 {
                warn!("usage: mode classic|enhanced|rtt-threshold|edpf");
                return CmdResponse::None;
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
                "edpf" => {
                    config.set_mode(SchedulingMode::Edpf);
                    info!("mode: edpf");
                }
                other => {
                    warn!(
                        "unknown mode '{}': use classic, enhanced, rtt-threshold, or edpf",
                        other
                    );
                }
            }
        }

        "quality" => {
            if parts.len() != 2 {
                warn!("usage: quality on|off");
                return CmdResponse::None;
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
                return CmdResponse::None;
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
                return CmdResponse::None;
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
            info!(
                "  critical hints: {} total, {} remaining",
                config.critical_hints_total(),
                config.critical_hint_remaining()
            );
        }

        "stats" => {
            if let Some(stats) = stats {
                let json = stats.to_json();
                info!("stats requested, returning {} bytes", json.len());
                return CmdResponse::Json(json);
            } else {
                warn!("stats not available (no stats provider)");
            }
        }

        "mark-critical" => {
            if parts.len() != 2 {
                warn!("usage: mark-critical <packet-count>");
                return CmdResponse::None;
            }
            match parts[1].parse::<u32>() {
                Ok(n) => {
                    config.add_critical_hint(n);
                    tracing::debug!("mark-critical: +{n} packets");
                }
                Err(_) => warn!("invalid mark-critical count: {}", parts[1]),
            }
        }

        other => {
            warn!("unknown command: {}", other);
        }
    }

    CmdResponse::None
}

#[cfg(unix)]
fn unix_socket_loop(config: &DynamicConfig, socket_path: &str, stats: &SharedStats) {
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
                let stats_clone = stats.clone();
                std::thread::spawn(move || {
                    handle_unix_client(config_clone, stream, stats_clone);
                });
            }
            Err(e) => {
                debug!("unix socket accept error: {}", e);
            }
        }
    }
}

#[cfg(unix)]
fn handle_unix_client(config: DynamicConfig, mut stream: UnixStream, stats: SharedStats) {
    // Clone stream for reading (we need separate read/write handles)
    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let reader = BufReader::new(read_stream);

    for line in reader.lines() {
        match line {
            Ok(cmd) => {
                let response = apply_cmd(&config, cmd.trim(), Some(&stats));
                if let CmdResponse::Json(json) = response {
                    // Write JSON response followed by newline
                    if let Err(e) = writeln!(stream, "{}", json) {
                        debug!("failed to write response: {}", e);
                        break;
                    }
                    if let Err(e) = stream.flush() {
                        debug!("failed to flush response: {}", e);
                        break;
                    }
                }
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

        apply_cmd(&config, "mode classic", None);
        assert_eq!(config.mode(), SchedulingMode::Classic);

        apply_cmd(&config, "mode enhanced", None);
        assert_eq!(config.mode(), SchedulingMode::Enhanced);

        apply_cmd(&config, "mode rtt-threshold", None);
        assert_eq!(config.mode(), SchedulingMode::RttThreshold);
    }

    #[test]
    fn test_quality_commands() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "quality off", None);
        assert!(!config.snapshot().quality_enabled);

        apply_cmd(&config, "quality on", None);
        assert!(config.snapshot().quality_enabled);
    }

    #[test]
    fn test_mark_critical_budget() {
        let config = DynamicConfig::new();
        assert_eq!(config.critical_hint_remaining(), 0);
        assert!(!config.consume_critical_hint());

        apply_cmd(&config, "mark-critical 3", None);
        assert_eq!(config.critical_hint_remaining(), 3);
        assert_eq!(config.critical_hints_total(), 1);

        assert!(config.consume_critical_hint());
        assert!(config.consume_critical_hint());
        assert!(config.consume_critical_hint());
        // Budget exhausted.
        assert!(!config.consume_critical_hint());
        assert_eq!(config.critical_hint_remaining(), 0);

        // Multiple hints accumulate.
        apply_cmd(&config, "mark-critical 2", None);
        apply_cmd(&config, "mark-critical 5", None);
        assert_eq!(config.critical_hint_remaining(), 7);
        assert_eq!(config.critical_hints_total(), 3);
    }

    #[test]
    fn test_mark_critical_invalid_input() {
        let config = DynamicConfig::new();
        apply_cmd(&config, "mark-critical", None);
        apply_cmd(&config, "mark-critical notanumber", None);
        apply_cmd(&config, "mark-critical 0", None);
        // None of those should have registered.
        assert_eq!(config.critical_hint_remaining(), 0);
        assert_eq!(config.critical_hints_total(), 0);
    }

    #[test]
    fn test_exploration_commands() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "explore on", None);
        assert!(config.snapshot().exploration_enabled);

        apply_cmd(&config, "explore off", None);
        assert!(!config.snapshot().exploration_enabled);
    }

    #[test]
    fn test_rtt_delta_commands() {
        let config = DynamicConfig::new();

        apply_cmd(&config, "rtt-delta 50", None);
        assert_eq!(config.snapshot().rtt_delta_ms, 50);

        apply_cmd(&config, "rtt-delta 100", None);
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
