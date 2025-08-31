//! SRTLA Sender Library
//!
//! This library provides functionality for SRTLA (SRT transport proxy with link
//! aggregation) sender implementation. It includes protocol handling,
//! connection management, and dynamic configuration toggles.

pub mod connection;
pub mod protocol;
pub mod registration;
pub mod sender;
pub mod toggles;
pub mod utils;

#[cfg(test)]
pub mod test_helpers;

#[cfg(test)]
pub mod tests;

// Re-export commonly used items
pub use connection::SrtlaConnection;
pub use protocol::*;
pub use registration::SrtlaRegistrationManager;
pub use toggles::DynamicToggles;
pub use utils::now_ms;
