//! SRTLA Sender Library
//!
//! This library provides functionality for SRTLA (SRT transport proxy with link
//! aggregation) sender implementation. It includes protocol handling,
//! connection management, and dynamic configuration.

// Use mimalloc as the global allocator for tests (non-Windows only). Excluded
// under miri: the batch_recv miri CI lane interprets the test binary, and miri
// cannot execute mimalloc's C FFI, so those runs fall back to miri's own
// allocator instead.
#[cfg(not(windows))]
#[cfg(all(test, not(miri)))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod config;
pub mod connection;
pub mod control;
pub mod control_socket;
pub mod ewma;
pub mod kalman;
pub mod metrics;
pub mod mode;
pub mod priority;
pub mod priority_listener;
// The wire protocol lives in its own dependency-free crate. Alias it as
// `protocol` so `crate::protocol::*` keeps resolving throughout the codebase.
pub use srtla_protocol as protocol;
pub mod registration;
// Scheduler / link selection. Core logic (mutually dependent with `connection`),
// so it lives top-level rather than under the `sender` shell.
pub mod selection;
pub mod sender;
pub mod stats;
pub mod subscriptions;
pub mod toml_config;
pub mod utils;

// Test helpers module - available when test-internals feature is enabled
#[cfg(any(test, feature = "test-internals"))]
pub mod test_helpers;

#[cfg(test)]
pub mod tests;

// Re-export commonly used items
pub use config::{ConfigSnapshot, DynamicConfig};
pub use connection::SrtlaConnection;
pub use mode::SchedulingMode;
pub use protocol::*;
pub use registration::SrtlaRegistrationManager;
pub use utils::now_ms;
