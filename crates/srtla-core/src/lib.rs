//! SRTLA sender core (sans-IO).
//!
//! The pure, socket-free heart of the SRTLA sender: connection state, path
//! selection, registration, congestion control, and the supporting numeric
//! helpers. No `tokio`, no sockets, no shell coupling. The parent `srtla_send`
//! crate owns all I/O and drives these types.

pub mod config_snapshot;
pub mod connection;
pub mod ewma;
pub mod kalman;
pub mod mode;
pub mod priority;
pub mod registration;
// Scheduler / link selection. Core logic (mutually dependent with `connection`).
pub mod selection;
pub mod utils;

// Test helpers (socket-free connection builders + tokio clock seam) - available
// when the test-internals feature is enabled so the parent crate can drive core
// types from its own cross-crate tests.
#[cfg(any(test, feature = "test-internals"))]
pub mod test_helpers;

// Re-export commonly used items
pub use config_snapshot::ConfigSnapshot;
pub use connection::SrtlaConnection;
pub use mode::SchedulingMode;
pub use registration::SrtlaRegistrationManager;
pub use utils::now_ms;
