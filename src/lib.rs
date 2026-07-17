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
pub mod control;
pub mod control_socket;
pub mod metrics;
// Uplink socket I/O (shell): batched UDP socket + egress binders.
pub mod net;
pub mod priority_listener;
pub mod sender;
pub mod stats;
pub mod subscriptions;
pub mod toml_config;

// Test helpers module - available when test-internals feature is enabled
#[cfg(any(test, feature = "test-internals"))]
pub mod test_helpers;

#[cfg(test)]
pub mod tests;

pub use config::{ConfigSnapshot, DynamicConfig};
