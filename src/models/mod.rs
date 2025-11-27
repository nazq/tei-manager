//! Model management module
//!
//! Provides functionality for:
//! - Detecting models in HuggingFace cache
//! - Downloading models from HuggingFace Hub
//! - Parsing model metadata from config.json
//! - Tracking model status (available, downloaded, verified)
//! - Smoke testing model loading

pub mod cache;
pub mod download;
pub mod loader;
pub mod metadata;
pub mod registry;

pub use cache::{get_cache_dir, get_model_cache_path, is_model_cached, list_cached_models};
pub use download::{download_model, download_model_to_cache};
pub use loader::{LoaderConfig, ModelLoader};
pub use metadata::{HfModelMetadata, parse_model_config};
pub use registry::{ModelEntry, ModelRegistry, ModelStatus};
