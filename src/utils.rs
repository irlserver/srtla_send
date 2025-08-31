//! Utility functions shared across the codebase

use std::time::{SystemTime, UNIX_EPOCH};

/// Get current time in milliseconds since Unix epoch
/// Returns 0 if system time is before Unix epoch (fallback behavior)
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_millis(0))
        .as_millis() as u64
}
