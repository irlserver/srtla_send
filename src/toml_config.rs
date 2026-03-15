//! Optional TOML file configuration for srtla_send.
//!
//! Loaded at startup via `--config <path>` and reloaded on SIGHUP.
//! All fields use `#[serde(default)]` so a partial config file is valid.

use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};

/// Top-level TOML configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TomlConfig {
    /// Scheduling mode: classic, enhanced, rtt-threshold, edpf.
    pub mode: String,
    /// Disable quality scoring.
    pub no_quality: bool,
    /// Enable connection exploration (enhanced only).
    pub exploration: bool,
    /// RTT delta threshold in ms (rtt-threshold mode).
    pub rtt_delta_ms: u32,

    // --- Congestion control ---
    /// RTT velocity threshold (ms/sample) above which window recovery is halved.
    pub rtt_velocity_gate: f64,

    // --- EDPF scheduler ---
    /// Velocity penalty factor for EDPF predicted arrival.
    pub edpf_velocity_penalty: f64,
    /// BDP overrun multiplier (links with in-flight > BDP * this are excluded).
    pub edpf_bdp_overrun_mult: f64,

    // --- Link lifecycle ---
    /// RTT probes required during warming phase before going Live.
    pub warming_rtt_probes: u32,
    /// Maximum time (ms) in warming phase before auto-promoting.
    pub warming_timeout_ms: u64,
    /// Quality threshold below which a Live link becomes Degraded.
    pub degraded_quality_threshold: f64,
    /// NAK burst count threshold for degradation.
    pub degraded_nak_burst_threshold: i32,
    /// Cooldown duration (ms) before re-entering Live from Degraded.
    pub cooldown_duration_ms: u64,

    // --- Selection ---
    /// Minimum time (ms) between connection switches.
    pub min_switch_interval_ms: u64,
    /// Switch hysteresis threshold (1.10 = 10% better required).
    pub switch_hysteresis: f64,
}

impl Default for TomlConfig {
    fn default() -> Self {
        Self {
            mode: "enhanced".to_string(),
            no_quality: false,
            exploration: false,
            rtt_delta_ms: 30,
            rtt_velocity_gate: 2.0,
            edpf_velocity_penalty: 0.005,
            edpf_bdp_overrun_mult: 1.5,
            warming_rtt_probes: 2,
            warming_timeout_ms: 5_000,
            degraded_quality_threshold: 0.5,
            degraded_nak_burst_threshold: 5,
            cooldown_duration_ms: 5_000,
            min_switch_interval_ms: 15,
            switch_hysteresis: 1.10,
        }
    }
}

impl TomlConfig {
    /// Load config from a TOML file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("failed to read {path:?}: {e}"))?;
        toml::from_str(&content).map_err(|e| format!("failed to parse {path:?}: {e}"))
    }

    /// Load config, logging errors and falling back to defaults.
    pub fn load_or_default(path: &Path) -> Self {
        match Self::load(path) {
            Ok(cfg) => {
                info!("loaded config from {}", path.display());
                cfg
            }
            Err(e) => {
                warn!("config load failed: {e}, using defaults");
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let cfg = TomlConfig::default();
        assert_eq!(cfg.mode, "enhanced");
        assert!(!cfg.no_quality);
        assert_eq!(cfg.rtt_delta_ms, 30);
        assert!((cfg.rtt_velocity_gate - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_partial_toml() {
        let toml_str = r#"
            mode = "edpf"
            rtt_velocity_gate = 3.5
        "#;
        let cfg: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.mode, "edpf");
        assert!((cfg.rtt_velocity_gate - 3.5).abs() < f64::EPSILON);
        // Defaults for unspecified fields
        assert_eq!(cfg.rtt_delta_ms, 30);
        assert!(!cfg.no_quality);
    }

    #[test]
    fn test_full_toml() {
        let toml_str = r#"
            mode = "classic"
            no_quality = true
            exploration = true
            rtt_delta_ms = 50
            rtt_velocity_gate = 1.0
            edpf_velocity_penalty = 0.01
            edpf_bdp_overrun_mult = 2.0
            warming_rtt_probes = 3
            warming_timeout_ms = 10000
            degraded_quality_threshold = 0.3
            degraded_nak_burst_threshold = 10
            cooldown_duration_ms = 8000
            min_switch_interval_ms = 30
            switch_hysteresis = 1.20
        "#;
        let cfg: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.mode, "classic");
        assert!(cfg.no_quality);
        assert_eq!(cfg.warming_rtt_probes, 3);
    }
}
