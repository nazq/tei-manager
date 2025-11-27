//! API request and response models

use crate::instance::{InstanceStatus, TeiInstance};
use serde::{Deserialize, Serialize};

/// Health check response
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Request to create a new instance
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateInstanceRequest {
    pub name: String,
    pub model_id: String,

    /// Port for the TEI instance
    /// If not provided, auto-allocated from instance_port_range in config
    /// Required if no port range is configured
    #[serde(default)]
    pub port: Option<u16>,

    #[serde(default)]
    pub max_batch_tokens: Option<u32>,

    #[serde(default)]
    pub max_concurrent_requests: Option<u32>,

    #[serde(default)]
    pub pooling: Option<String>,

    #[serde(default)]
    pub gpu_id: Option<u32>,

    #[serde(default)]
    pub prometheus_port: Option<u16>,

    /// Override startup timeout for this instance (seconds)
    /// If not provided, uses global startup_timeout_secs from manager config
    /// Use for large models that need more time to download/load
    #[serde(default)]
    pub startup_timeout_secs: Option<u64>,

    #[serde(default)]
    pub extra_args: Option<Vec<String>>,
}

/// Instance information response
#[derive(Debug, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub name: String,
    pub model_id: String,
    pub port: u16,
    pub status: InstanceStatus,
    pub pid: Option<u32>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub uptime_secs: Option<u64>,
    pub restarts: u32,
    pub health_check_failures: u32,
    pub last_health_check: Option<chrono::DateTime<chrono::Utc>>,
    pub gpu_id: Option<u32>,
    pub prometheus_port: Option<u16>,
}

impl InstanceInfo {
    /// Create InstanceInfo from TeiInstance
    pub async fn from_instance(instance: &TeiInstance) -> Self {
        let status = *instance.status.read().await;
        let stats = instance.stats.read().await;
        let pid = instance.pid().await;

        let uptime_secs = stats
            .started_at
            .map(|start| (chrono::Utc::now() - start).num_seconds() as u64);

        Self {
            name: instance.config.name.clone(),
            model_id: instance.config.model_id.clone(),
            port: instance.config.port,
            status,
            pid,
            created_at: instance.config.created_at,
            uptime_secs,
            restarts: stats.restarts,
            health_check_failures: stats.health_check_failures,
            last_health_check: stats.last_health_check,
            gpu_id: instance.config.gpu_id,
            prometheus_port: instance.config.prometheus_port,
        }
    }
}

/// Log file response with Python-style slicing
#[derive(Debug, Serialize, Deserialize)]
pub struct LogsResponse {
    pub lines: Vec<String>,
    pub start: usize,
    pub end: usize,
    pub total_lines: usize,
}

// ============================================================================
// Model Management Types
// ============================================================================

use crate::models::{HfModelMetadata, ModelEntry, ModelStatus};

/// Model information response
#[derive(Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    /// HuggingFace model ID
    pub model_id: String,
    /// Current status
    pub status: ModelStatus,
    /// Whether model is downloaded to cache
    pub downloaded: bool,
    /// Cache path if downloaded
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,
    /// Cache size in bytes if downloaded
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_size_bytes: Option<u64>,
    /// When model was last verified
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<chrono::DateTime<chrono::Utc>>,
    /// Error message if verification failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_error: Option<String>,
    /// Model metadata from config.json
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HfModelMetadata>,
    /// When this model was added to registry
    pub added_at: chrono::DateTime<chrono::Utc>,
}

impl From<ModelEntry> for ModelInfo {
    fn from(entry: ModelEntry) -> Self {
        Self {
            model_id: entry.model_id,
            status: entry.status,
            downloaded: entry.cache_info.is_some(),
            cache_path: entry
                .cache_info
                .as_ref()
                .map(|c| c.path.to_string_lossy().to_string()),
            cache_size_bytes: entry.cache_info.as_ref().map(|c| c.size_bytes),
            last_verified: entry.last_verified,
            verification_error: entry.verification_error,
            metadata: entry.metadata,
            added_at: entry.added_at,
        }
    }
}

/// Request to add a model to the registry
#[derive(Debug, Serialize, Deserialize)]
pub struct AddModelRequest {
    /// HuggingFace model ID (e.g., "BAAI/bge-small-en-v1.5")
    pub model_id: String,
}
