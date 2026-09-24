//! Optional TOML file configuration for srtla_send.
//!
//! Loaded once at startup via `--config <path>`. Each key mirrors the CLI flag
//! of the same name, and a flag typed on the command line wins over the file.
//! Unknown keys fail the load, so a misspelled or retired key stops startup
//! instead of silently doing nothing.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};
use srtla_core::mode::SchedulingMode;

/// Values read from the config file. `None` means the key was absent.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TomlConfig {
    #[serde(deserialize_with = "deserialize_mode")]
    pub mode: Option<SchedulingMode>,
    pub no_quality: Option<bool>,
    pub no_stall_deselect: Option<bool>,
    pub stall_min_in_flight: Option<i32>,
    pub stall_ack_stale_ms: Option<u64>,
    pub conn_timeout_ms: Option<u64>,
}

// `SchedulingMode` is a pure core type with no serde dependency, so the file
// goes through the same `FromStr` the CLI uses.
fn deserialize_mode<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SchedulingMode>, D::Error> {
    String::deserialize(d)?
        .parse()
        .map(Some)
        .map_err(serde::de::Error::custom)
}

impl TomlConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("failed to parse config file {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_keys_are_none() {
        let cfg: TomlConfig = toml::from_str("mode = \"classic\"").unwrap();
        assert_eq!(cfg.mode, Some(SchedulingMode::Classic));
        assert_eq!(cfg.no_quality, None);
        assert_eq!(cfg.conn_timeout_ms, None);
    }

    #[test]
    fn full_toml() {
        let toml_str = r#"
            mode = "enhanced"
            no_quality = true
            no_stall_deselect = true
            stall_min_in_flight = 64
            stall_ack_stale_ms = 1500
            conn_timeout_ms = 8000
        "#;
        let cfg: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.mode, Some(SchedulingMode::Enhanced));
        assert_eq!(cfg.no_quality, Some(true));
        assert_eq!(cfg.no_stall_deselect, Some(true));
        assert_eq!(cfg.stall_min_in_flight, Some(64));
        assert_eq!(cfg.stall_ack_stale_ms, Some(1500));
        assert_eq!(cfg.conn_timeout_ms, Some(8000));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = toml::from_str::<TomlConfig>("switch_hysteresis = 1.2").unwrap_err();
        assert!(err.to_string().contains("switch_hysteresis"), "{err}");
    }

    #[test]
    fn invalid_mode_is_rejected() {
        let err = toml::from_str::<TomlConfig>("mode = \"fastest\"").unwrap_err();
        assert!(err.to_string().contains("fastest"), "{err}");
    }
}
