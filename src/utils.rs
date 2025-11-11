//! Utility functions shared across the codebase

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

/// Static startup instant for stable epoch-based timing calculations
/// This is initialized once at program startup and used for periodic operations
/// that need to be based on a stable reference point.
pub static STARTUP_INSTANT: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Get current time in milliseconds since Unix epoch
/// Returns 0 if system time is before Unix epoch (fallback behavior)
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_millis(0))
        .as_millis() as u64
}

/// Get elapsed milliseconds since program startup
/// Uses the stable STARTUP_INSTANT for consistent periodic timing
pub fn elapsed_ms() -> u64 {
    STARTUP_INSTANT.elapsed().as_millis() as u64
}

/// Get elapsed milliseconds since program startup for a given Instant
/// Uses the stable STARTUP_INSTANT for consistent periodic timing
pub fn instant_to_elapsed_ms(instant: Instant) -> u64 {
    STARTUP_INSTANT
        .elapsed()
        .saturating_sub(instant.elapsed())
        .as_millis() as u64
}
