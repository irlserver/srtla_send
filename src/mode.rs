//! Scheduling mode enum for SRTLA connection selection.

use std::fmt;

/// Scheduling mode for connection selection.
///
/// Determines which algorithm is used to select the next connection
/// for sending SRT packets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SchedulingMode {
    /// Classic mode: pure capacity-based selection (window / in_flight).
    /// No quality scoring, no dampening, no exploration.
    /// Matches the original C implementation behavior.
    Classic,

    /// Enhanced mode (default): quality-aware selection with dampening.
    /// Supports quality scoring and smart exploration.
    #[default]
    Enhanced,

    /// RTT-threshold mode: groups links by RTT proximity.
    /// Selects from "fast" links (within rtt_delta of minimum).
    /// Supports quality scoring within the fast group.
    RttThreshold,
}

impl SchedulingMode {
    /// Convert to u8 for atomic storage.
    pub const fn as_u8(self) -> u8 {
        match self {
            SchedulingMode::Classic => 0,
            SchedulingMode::Enhanced => 1,
            SchedulingMode::RttThreshold => 2,
        }
    }

    /// Convert from u8, defaulting to Enhanced for invalid values.
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => SchedulingMode::Classic,
            1 => SchedulingMode::Enhanced,
            2 => SchedulingMode::RttThreshold,
            _ => SchedulingMode::Enhanced,
        }
    }

    /// Check if this mode is classic.
    pub const fn is_classic(self) -> bool {
        matches!(self, SchedulingMode::Classic)
    }

    /// Check if this mode is enhanced.
    pub const fn is_enhanced(self) -> bool {
        matches!(self, SchedulingMode::Enhanced)
    }
}

impl fmt::Display for SchedulingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedulingMode::Classic => write!(f, "classic"),
            SchedulingMode::Enhanced => write!(f, "enhanced"),
            SchedulingMode::RttThreshold => write!(f, "rtt-threshold"),
        }
    }
}

impl std::str::FromStr for SchedulingMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "classic" => Ok(SchedulingMode::Classic),
            "enhanced" => Ok(SchedulingMode::Enhanced),
            "rtt-threshold" => Ok(SchedulingMode::RttThreshold),
            _ => Err(format!(
                "invalid mode '{}': use classic, enhanced, or rtt-threshold",
                s
            )),
        }
    }
}

impl clap::ValueEnum for SchedulingMode {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            SchedulingMode::Classic,
            SchedulingMode::Enhanced,
            SchedulingMode::RttThreshold,
        ]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        match self {
            SchedulingMode::Classic => Some(clap::builder::PossibleValue::new("classic")),
            SchedulingMode::Enhanced => Some(clap::builder::PossibleValue::new("enhanced")),
            SchedulingMode::RttThreshold => {
                Some(clap::builder::PossibleValue::new("rtt-threshold"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_default() {
        assert_eq!(SchedulingMode::default(), SchedulingMode::Enhanced);
    }

    #[test]
    fn test_mode_u8_roundtrip() {
        for mode in [
            SchedulingMode::Classic,
            SchedulingMode::Enhanced,
            SchedulingMode::RttThreshold,
        ] {
            assert_eq!(SchedulingMode::from_u8(mode.as_u8()), mode);
        }
    }

    #[test]
    fn test_mode_from_str() {
        assert_eq!(
            "classic".parse::<SchedulingMode>().unwrap(),
            SchedulingMode::Classic
        );
        assert_eq!(
            "enhanced".parse::<SchedulingMode>().unwrap(),
            SchedulingMode::Enhanced
        );
        assert_eq!(
            "rtt-threshold".parse::<SchedulingMode>().unwrap(),
            SchedulingMode::RttThreshold
        );
        assert!("invalid".parse::<SchedulingMode>().is_err());
    }

    #[test]
    fn test_mode_display() {
        assert_eq!(format!("{}", SchedulingMode::Classic), "classic");
        assert_eq!(format!("{}", SchedulingMode::Enhanced), "enhanced");
        assert_eq!(format!("{}", SchedulingMode::RttThreshold), "rtt-threshold");
    }

    #[test]
    fn test_mode_checks() {
        assert!(SchedulingMode::Classic.is_classic());
        assert!(!SchedulingMode::Classic.is_enhanced());

        assert!(!SchedulingMode::Enhanced.is_classic());
        assert!(SchedulingMode::Enhanced.is_enhanced());

        assert!(!SchedulingMode::RttThreshold.is_classic());
        assert!(!SchedulingMode::RttThreshold.is_enhanced());
    }
}
