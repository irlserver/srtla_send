//! SRTLA Sender Library
//!
//! This library provides functionality for SRTLA (SRT transport proxy with link
//! aggregation) sender implementation. It includes protocol handling,
//! connection management, and dynamic configuration.

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
pub mod version;

// Test helpers module - available when test-internals feature is enabled
#[cfg(any(test, feature = "test-internals"))]
pub mod test_helpers;

#[cfg(test)]
pub mod tests;

pub use config::{ConfigSnapshot, DynamicConfig};
