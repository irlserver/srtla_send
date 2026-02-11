//! Network simulation for integration testing under realistic impairment.
//!
//! Uses Linux network namespaces and `tc netem`/`tbf` to create isolated
//! virtual networks with configurable delay, loss, bandwidth, and jitter.
//!
//! # Modules
//!
//! - [`topology`]: Namespace and veth link management (RAII cleanup on drop)
//! - [`impairment`]: `tc netem`/`tbf` configuration and application
//! - [`scenario`]: Deterministic random-walk impairment generator
//! - [`test_util`]: Privilege checks and unique name generation for tests

pub mod impairment;
pub mod scenario;
pub mod test_util;
pub mod topology;

pub use impairment::{GemodelConfig, ImpairmentConfig, apply_impairment};
pub use scenario::{LinkScenarioConfig, Scenario, ScenarioConfig, ScenarioFrame};
pub use test_util::{check_privileges, unique_ns_name};
pub use topology::Namespace;
