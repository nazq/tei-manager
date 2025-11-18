//! TEI Manager - Dynamic TEI instance manager
//!
//! A lightweight Rust service that dynamically manages multiple TEI (Text Embeddings Inference)
//! instances on a single GPU host.

pub mod api;
pub mod config;
pub mod error;
pub mod health;
pub mod instance;
pub mod metrics;
pub mod registry;
pub mod state;

pub use config::{InstanceConfig, ManagerConfig};
pub use error::ApiError;
pub use health::HealthMonitor;
pub use instance::{InstanceStats, InstanceStatus, TeiInstance};
pub use registry::Registry;
pub use state::StateManager;
