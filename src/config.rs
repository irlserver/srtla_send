//! Runtime configuration for SRTLA sender.
//!
//! `DynamicConfig` holds atomic state that can be flipped at runtime. The
//! actual wire protocol lives in [`crate::control`] — this module exposes
//! plain getters/setters that the control dispatcher calls into.

use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, Ordering};

use crate::control::dispatch;
use crate::mode::SchedulingMode;
use crate::priority::CriticalWindow;
use crate::stats::SharedStats;

/// In-flight packet backlog at or above which a link is a stall candidate
/// under the `stall_deselect` guard (default on).
pub const STALL_MIN_IN_FLIGHT_PACKETS: i32 = 32;

/// Staleness window (ms) for a link's last delivery proof (earned-ACK or
/// keepalive-RTT sample) under `stall_deselect`. A stall candidate whose last
/// proof is older than this is treated as stalled. Kept well below
/// `CONN_TIMEOUT` (15 s): deselect is a selection penalty ONLY, never a
/// liveness/timeout shortcut.
pub const STALL_ACK_STALE_MS: u64 = 3000;

/// Snapshot of configuration for efficient hot-path access.
/// Call `DynamicConfig::snapshot()` once per select iteration to avoid
/// multiple atomic loads per packet in the hot path.
#[derive(Clone, Copy, Debug)]
pub struct ConfigSnapshot {
    pub mode: SchedulingMode,
    pub quality_enabled: bool,
    pub exploration_enabled: bool,
    /// Stalled-link deselect (default ON). On, the selection layer excludes a
    /// link whose in-flight backlog is high while its last delivery proof has
    /// gone stale, provided at least one healthier link can carry the traffic.
    /// Off (`--no-stall-deselect`), selection is byte-for-byte unchanged.
    pub stall_deselect: bool,
    /// In-flight threshold for `stall_deselect` (default [`STALL_MIN_IN_FLIGHT_PACKETS`]).
    pub stall_min_in_flight: i32,
    /// Delivery-proof staleness window in ms for `stall_deselect`
    /// (default [`STALL_ACK_STALE_MS`]).
    pub stall_ack_stale_ms: u64,
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            stall_deselect: true,
            stall_min_in_flight: STALL_MIN_IN_FLIGHT_PACKETS,
            stall_ack_stale_ms: STALL_ACK_STALE_MS,
        }
    }
}

impl ConfigSnapshot {
    /// Check if quality scoring is effective for the current mode.
    /// Quality scoring only applies to enhanced mode.
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
    stall_deselect: Arc<AtomicBool>,
    stall_min_in_flight: Arc<AtomicI32>,
    stall_ack_stale_ms: Arc<AtomicU64>,
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
            stall_deselect: Arc::new(AtomicBool::new(true)),
            stall_min_in_flight: Arc::new(AtomicI32::new(STALL_MIN_IN_FLIGHT_PACKETS)),
            stall_ack_stale_ms: Arc::new(AtomicU64::new(STALL_ACK_STALE_MS)),
        }
    }

    /// Create config from CLI arguments.
    pub fn from_cli(
        mode: SchedulingMode,
        no_quality: bool,
        exploration: bool,
        no_stall_deselect: bool,
        stall_min_in_flight: i32,
        stall_ack_stale_ms: u64,
    ) -> Self {
        Self {
            mode: Arc::new(AtomicU8::new(mode.as_u8())),
            quality_enabled: Arc::new(AtomicBool::new(!no_quality)),
            exploration_enabled: Arc::new(AtomicBool::new(exploration)),
            stall_deselect: Arc::new(AtomicBool::new(!no_stall_deselect)),
            stall_min_in_flight: Arc::new(AtomicI32::new(stall_min_in_flight)),
            stall_ack_stale_ms: Arc::new(AtomicU64::new(stall_ack_stale_ms)),
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
            stall_deselect: self.stall_deselect.load(Ordering::Relaxed),
            stall_min_in_flight: self.stall_min_in_flight.load(Ordering::Relaxed),
            stall_ack_stale_ms: self.stall_ack_stale_ms.load(Ordering::Relaxed),
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

    /// Toggle the stalled-link deselect guard at runtime.
    pub fn set_stall_deselect(&self, enabled: bool) {
        self.stall_deselect.store(enabled, Ordering::Relaxed);
    }
}

/// Spawn the stdin command reader in a std::thread. Stdin on Linux
/// doesn't have a clean async story — easier to stay blocking here.
/// The Unix control socket, which actually needs subscriptions, lives
/// in an async tokio task launched by main instead.
pub fn spawn_stdin_listener(
    config: DynamicConfig,
    stats: SharedStats,
    critical_window: CriticalWindow,
) {
    std::thread::spawn(move || {
        let reader = BufReader::new(std::io::stdin());
        for line in reader.lines().map_while(Result::ok) {
            if let Some(resp) = dispatch(&config, Some(&stats), Some(&critical_window), line.trim())
            {
                // Responses on stdin just go to stdout so scripts can pipe.
                println!("{}", resp.to_json());
            }
        }
    });
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
    }

    #[test]
    fn test_config_from_cli() {
        let config = DynamicConfig::from_cli(
            SchedulingMode::Classic,
            true,
            true,
            false,
            STALL_MIN_IN_FLIGHT_PACKETS,
            STALL_ACK_STALE_MS,
        );
        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Classic);
        assert!(!snap.quality_enabled); // no_quality=true means disabled
        assert!(snap.exploration_enabled);
        assert!(snap.stall_deselect); // on by default (no_stall_deselect=false)
    }

    #[test]
    fn test_effective_quality() {
        // Classic mode - quality never effective, exploration never effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: true,
            exploration_enabled: true,
            ..ConfigSnapshot::default()
        };
        assert!(!snap.effective_quality_enabled());
        assert!(!snap.effective_exploration_enabled());

        // Enhanced mode - both can be effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: true,
            ..ConfigSnapshot::default()
        };
        assert!(snap.effective_quality_enabled());
        assert!(snap.effective_exploration_enabled());
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
