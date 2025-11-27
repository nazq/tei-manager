//! Model registry for tracking known models and their status

use super::cache::{get_cache_size, get_model_cache_path, is_model_cached, list_cached_models};
use super::metadata::{HfModelMetadata, parse_model_config};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Status of a model in the registry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    /// Model is known but not downloaded
    Available,
    /// Model is currently being downloaded
    Downloading,
    /// Model is downloaded to HF cache
    Downloaded,
    /// Model is currently being loaded (smoke test in progress)
    Loading,
    /// Model has been verified (smoke test passed)
    Verified,
    /// Model failed to load (smoke test failed)
    Failed,
}

impl std::fmt::Display for ModelStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Available => write!(f, "available"),
            Self::Downloading => write!(f, "downloading"),
            Self::Downloaded => write!(f, "downloaded"),
            Self::Loading => write!(f, "loading"),
            Self::Verified => write!(f, "verified"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// Information about a cached model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheInfo {
    /// Path to the model's snapshot directory
    pub path: PathBuf,
    /// Total size of cached files in bytes
    pub size_bytes: u64,
}

/// Entry for a model in the registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// HuggingFace model ID (e.g., "BAAI/bge-small-en-v1.5")
    pub model_id: String,
    /// Current status
    pub status: ModelStatus,
    /// Cache information if downloaded
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_info: Option<CacheInfo>,
    /// Metadata from config.json
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HfModelMetadata>,
    /// When the model was last verified (smoke test)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<DateTime<Utc>>,
    /// Error message if verification failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_error: Option<String>,
    /// When this entry was added to the registry
    pub added_at: DateTime<Utc>,
}

impl ModelEntry {
    /// Create a new model entry
    pub fn new(model_id: String) -> Self {
        Self {
            model_id,
            status: ModelStatus::Available,
            cache_info: None,
            metadata: None,
            last_verified: None,
            verification_error: None,
            added_at: Utc::now(),
        }
    }

    /// Update entry with cache information
    pub fn with_cache_info(mut self) -> Self {
        if is_model_cached(&self.model_id)
            && let Some(path) = get_model_cache_path(&self.model_id)
        {
            let size_bytes = get_cache_size(&self.model_id).unwrap_or(0);
            self.cache_info = Some(CacheInfo { path, size_bytes });
            self.status = ModelStatus::Downloaded;
        }
        self
    }

    /// Update entry with metadata from config.json
    pub fn with_metadata(mut self) -> Self {
        if let Some(ref cache_info) = self.cache_info {
            self.metadata = parse_model_config(&cache_info.path);
        }
        self
    }

    /// Refresh cache and metadata information
    pub fn refresh(&mut self) {
        if is_model_cached(&self.model_id) {
            if let Some(path) = get_model_cache_path(&self.model_id) {
                let size_bytes = get_cache_size(&self.model_id).unwrap_or(0);
                self.cache_info = Some(CacheInfo {
                    path: path.clone(),
                    size_bytes,
                });
                self.metadata = parse_model_config(&path);

                // Update status to Downloaded if not already verified/failed
                // This handles both Available -> Downloaded and Downloading -> Downloaded
                if self.status == ModelStatus::Available || self.status == ModelStatus::Downloading
                {
                    self.status = ModelStatus::Downloaded;
                }
            }
        } else {
            self.cache_info = None;
            self.metadata = None;
            self.status = ModelStatus::Available;
        }
    }
}

/// Registry for tracking models
pub struct ModelRegistry {
    models: Arc<RwLock<HashMap<String, ModelEntry>>>,
}

impl ModelRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            models: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Initialize registry with configured models and discover cached models
    pub async fn init(configured_models: Vec<String>) -> Self {
        let registry = Self::new();

        // Add configured models
        for model_id in configured_models {
            registry.add_model(model_id).await;
        }

        // Discover cached models
        registry.discover_cached_models().await;

        registry
    }

    /// Add a model to the registry
    pub async fn add_model(&self, model_id: String) -> ModelEntry {
        let entry = ModelEntry::new(model_id.clone())
            .with_cache_info()
            .with_metadata();

        let mut models = self.models.write().await;
        models.insert(model_id.clone(), entry.clone());

        entry
    }

    /// Get a model entry by ID
    pub async fn get(&self, model_id: &str) -> Option<ModelEntry> {
        let models = self.models.read().await;
        models.get(model_id).cloned()
    }

    /// Get a model entry, refreshing cache info first
    pub async fn get_refreshed(&self, model_id: &str) -> Option<ModelEntry> {
        let mut models = self.models.write().await;

        if let Some(entry) = models.get_mut(model_id) {
            entry.refresh();
            return Some(entry.clone());
        }

        None
    }

    /// List all models
    pub async fn list(&self) -> Vec<ModelEntry> {
        let models = self.models.read().await;
        let mut entries: Vec<_> = models.values().cloned().collect();
        entries.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        entries
    }

    /// Check if a model is in the registry
    pub async fn contains(&self, model_id: &str) -> bool {
        let models = self.models.read().await;
        models.contains_key(model_id)
    }

    /// Update model status
    pub async fn set_status(&self, model_id: &str, status: ModelStatus) {
        let mut models = self.models.write().await;
        if let Some(entry) = models.get_mut(model_id) {
            entry.status = status;
        }
    }

    /// Mark model as verified
    pub async fn set_verified(&self, model_id: &str) {
        let mut models = self.models.write().await;
        if let Some(entry) = models.get_mut(model_id) {
            entry.status = ModelStatus::Verified;
            entry.last_verified = Some(Utc::now());
            entry.verification_error = None;
        }
    }

    /// Mark model as failed with error message
    pub async fn set_failed(&self, model_id: &str, error: String) {
        let mut models = self.models.write().await;
        if let Some(entry) = models.get_mut(model_id) {
            entry.status = ModelStatus::Failed;
            entry.verification_error = Some(error);
        }
    }

    /// Discover and add cached models not already in registry
    pub async fn discover_cached_models(&self) {
        let cached = list_cached_models();

        for model_id in cached {
            if !self.contains(&model_id).await {
                self.add_model(model_id).await;
            }
        }
    }

    /// Refresh cache info for all models
    pub async fn refresh_all(&self) {
        let mut models = self.models.write().await;
        for entry in models.values_mut() {
            entry.refresh();
        }
    }

    /// Get count of models in registry
    pub async fn count(&self) -> usize {
        let models = self.models.read().await;
        models.len()
    }

    /// Get count of downloaded models
    pub async fn downloaded_count(&self) -> usize {
        let models = self.models.read().await;
        models.values().filter(|e| e.cache_info.is_some()).count()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_registry() {
        let registry = ModelRegistry::new();
        assert_eq!(registry.count().await, 0);
    }

    #[tokio::test]
    async fn test_add_model() {
        let registry = ModelRegistry::new();
        let entry = registry.add_model("test/model".to_string()).await;

        assert_eq!(entry.model_id, "test/model");
        assert_eq!(entry.status, ModelStatus::Available);
        assert!(registry.contains("test/model").await);
    }

    #[tokio::test]
    async fn test_list_models() {
        let registry = ModelRegistry::new();
        registry.add_model("b/model".to_string()).await;
        registry.add_model("a/model".to_string()).await;

        let models = registry.list().await;
        assert_eq!(models.len(), 2);
        // Should be sorted
        assert_eq!(models[0].model_id, "a/model");
        assert_eq!(models[1].model_id, "b/model");
    }

    #[tokio::test]
    async fn test_set_status() {
        let registry = ModelRegistry::new();
        registry.add_model("test/model".to_string()).await;

        registry
            .set_status("test/model", ModelStatus::Loading)
            .await;

        let entry = registry.get("test/model").await.unwrap();
        assert_eq!(entry.status, ModelStatus::Loading);
    }

    #[tokio::test]
    async fn test_set_verified() {
        let registry = ModelRegistry::new();
        registry.add_model("test/model".to_string()).await;

        registry.set_verified("test/model").await;

        let entry = registry.get("test/model").await.unwrap();
        assert_eq!(entry.status, ModelStatus::Verified);
        assert!(entry.last_verified.is_some());
    }

    #[tokio::test]
    async fn test_set_failed() {
        let registry = ModelRegistry::new();
        registry.add_model("test/model".to_string()).await;

        registry
            .set_failed("test/model", "out of memory".to_string())
            .await;

        let entry = registry.get("test/model").await.unwrap();
        assert_eq!(entry.status, ModelStatus::Failed);
        assert_eq!(entry.verification_error, Some("out of memory".to_string()));
    }

    #[test]
    fn test_model_status_display() {
        assert_eq!(ModelStatus::Available.to_string(), "available");
        assert_eq!(ModelStatus::Downloading.to_string(), "downloading");
        assert_eq!(ModelStatus::Downloaded.to_string(), "downloaded");
        assert_eq!(ModelStatus::Loading.to_string(), "loading");
        assert_eq!(ModelStatus::Verified.to_string(), "verified");
        assert_eq!(ModelStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn test_model_entry_new() {
        let entry = ModelEntry::new("test/model".to_string());
        assert_eq!(entry.model_id, "test/model");
        assert_eq!(entry.status, ModelStatus::Available);
        assert!(entry.cache_info.is_none());
        assert!(entry.metadata.is_none());
        assert!(entry.last_verified.is_none());
        assert!(entry.verification_error.is_none());
    }

    #[test]
    fn test_model_entry_with_cache_info_not_cached() {
        let entry = ModelEntry::new("nonexistent/model-12345".to_string()).with_cache_info();
        // Model is not cached, so should remain Available
        assert_eq!(entry.status, ModelStatus::Available);
        assert!(entry.cache_info.is_none());
    }

    #[test]
    fn test_model_entry_with_metadata_no_cache() {
        let entry = ModelEntry::new("nonexistent/model-12345".to_string()).with_metadata();
        // No cache info means no metadata
        assert!(entry.metadata.is_none());
    }

    #[test]
    fn test_model_entry_refresh_not_cached() {
        let mut entry = ModelEntry::new("nonexistent/model-12345".to_string());
        entry.status = ModelStatus::Downloaded; // Pretend it was downloaded
        entry.refresh();
        // Should reset to Available since not actually cached
        assert_eq!(entry.status, ModelStatus::Available);
        assert!(entry.cache_info.is_none());
    }

    #[test]
    fn test_cache_info_serialize() {
        use std::path::PathBuf;
        let cache_info = CacheInfo {
            path: PathBuf::from("/test/path"),
            size_bytes: 12345,
        };
        let json = serde_json::to_string(&cache_info).unwrap();
        assert!(json.contains("12345"));
    }

    #[test]
    fn test_model_entry_serialize() {
        let entry = ModelEntry::new("test/model".to_string());
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("test/model"));
        assert!(json.contains("available"));
        // Optional fields should be skipped when None
        assert!(!json.contains("cache_info"));
        assert!(!json.contains("metadata"));
    }

    #[tokio::test]
    async fn test_registry_refresh_all() {
        let registry = ModelRegistry::new();
        registry.add_model("test1/model".to_string()).await;
        registry.add_model("test2/model".to_string()).await;
        // Should not panic even though models aren't actually cached
        registry.refresh_all().await;
    }

    #[tokio::test]
    async fn test_registry_add_model_returns_entry() {
        let registry = ModelRegistry::new();
        let entry = registry.add_model("test/model".to_string()).await;
        assert_eq!(entry.model_id, "test/model");
        assert_eq!(entry.status, ModelStatus::Available);
    }

    #[tokio::test]
    async fn test_registry_add_duplicate() {
        let registry = ModelRegistry::new();
        let entry1 = registry.add_model("test/model".to_string()).await;
        let entry2 = registry.add_model("test/model".to_string()).await;
        // Should return same entry (idempotent)
        assert_eq!(entry1.model_id, entry2.model_id);
    }

    #[tokio::test]
    async fn test_set_status_nonexistent_model() {
        let registry = ModelRegistry::new();
        // Should not panic when model doesn't exist
        registry
            .set_status("nonexistent/model", ModelStatus::Loading)
            .await;
        // Model should still not exist
        assert!(registry.get("nonexistent/model").await.is_none());
    }

    #[tokio::test]
    async fn test_set_verified_nonexistent_model() {
        let registry = ModelRegistry::new();
        // Should not panic when model doesn't exist
        registry.set_verified("nonexistent/model").await;
        assert!(registry.get("nonexistent/model").await.is_none());
    }

    #[tokio::test]
    async fn test_set_failed_nonexistent_model() {
        let registry = ModelRegistry::new();
        // Should not panic when model doesn't exist
        registry
            .set_failed("nonexistent/model", "error".to_string())
            .await;
        assert!(registry.get("nonexistent/model").await.is_none());
    }
}
