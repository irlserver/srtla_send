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
// The hot-path snapshot type + its default constants are core; they live in
// `config_snapshot` (free of this module's control/stats coupling). Re-exported
// here so the existing `crate::config::{ConfigSnapshot, STALL_*}` paths keep
// resolving across the codebase.
pub use crate::config_snapshot::{ConfigSnapshot, STALL_ACK_STALE_MS, STALL_MIN_IN_FLIGHT_PACKETS};

/// Dynamic configuration that can be modified at runtime.
/// Uses atomic types for lock-free concurrent access.
#[derive(Clone)]
pub struct DynamicConfig {
    mode: Arc<AtomicU8>,
    quality_enabled: Arc<AtomicBool>,
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
            stall_deselect: Arc::new(AtomicBool::new(true)),
            stall_min_in_flight: Arc::new(AtomicI32::new(STALL_MIN_IN_FLIGHT_PACKETS)),
            stall_ack_stale_ms: Arc::new(AtomicU64::new(STALL_ACK_STALE_MS)),
        }
    }

    /// Create config from CLI arguments.
    pub fn from_cli(
        mode: SchedulingMode,
        no_quality: bool,
        no_stall_deselect: bool,
        stall_min_in_flight: i32,
        stall_ack_stale_ms: u64,
    ) -> Self {
        Self {
            mode: Arc::new(AtomicU8::new(mode.as_u8())),
            quality_enabled: Arc::new(AtomicBool::new(!no_quality)),
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
    }

    #[test]
    fn test_config_from_cli() {
        let config = DynamicConfig::from_cli(
            SchedulingMode::Classic,
            true,
            false,
            STALL_MIN_IN_FLIGHT_PACKETS,
            STALL_ACK_STALE_MS,
        );
        let snap = config.snapshot();
        assert_eq!(snap.mode, SchedulingMode::Classic);
        assert!(!snap.quality_enabled); // no_quality=true means disabled
        assert!(snap.stall_deselect); // on by default (no_stall_deselect=false)
    }

    #[test]
    fn test_effective_quality() {
        // Classic mode - quality scoring never effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: true,
            ..ConfigSnapshot::default()
        };
        assert!(!snap.effective_quality_enabled());

        // Enhanced mode - effective
        let snap = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            ..ConfigSnapshot::default()
        };
        assert!(snap.effective_quality_enabled());
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
