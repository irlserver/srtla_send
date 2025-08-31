//! Utility functions shared across the codebase

use std::time::{SystemTime, UNIX_EPOCH};

/// Get current time in milliseconds since Unix epoch
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
