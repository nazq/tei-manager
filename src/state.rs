//! State persistence for instance configurations

use crate::config::InstanceConfig;
use crate::registry::Registry;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;

// ============================================================================
// Trait Definitions
// ============================================================================

/// Trait for storage backend operations
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Save content to a file path atomically
    async fn save(&self, path: &Path, content: &str) -> Result<()>;

    /// Load content from a file path
    /// Returns None if file doesn't exist
    async fn load(&self, path: &Path) -> Result<Option<String>>;

    /// Check if a file exists
    fn exists(&self, path: &Path) -> bool;
}

// ============================================================================
// Production Implementation
// ============================================================================

/// Production storage backend using tokio::fs
pub struct FileSystemStorage;

impl FileSystemStorage {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileSystemStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageBackend for FileSystemStorage {
    async fn save(&self, path: &Path, content: &str) -> Result<()> {
        // Atomic write: write to temp file, then rename
        let temp_file = path.with_extension("tmp");

        let mut file = fs::File::create(&temp_file)
            .await
            .context("Failed to create temp state file")?;
        file.write_all(content.as_bytes())
            .await
            .context("Failed to write state file")?;
        file.sync_all().await.context("Failed to sync state file")?;

        fs::rename(&temp_file, path)
            .await
            .context("Failed to rename temp state file")?;

        Ok(())
    }

    async fn load(&self, path: &Path) -> Result<Option<String>> {
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read state file: {:?}", path))?;

        Ok(Some(content))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

// ============================================================================
// State Manager with Dependency Injection
// ============================================================================

/// State manager for persisting instance configurations
pub struct StateManager {
    state_file: PathBuf,
    registry: Arc<Registry>,
    tei_binary_path: Arc<str>,
    storage: Arc<dyn StorageBackend>,
    /// Guard to prevent concurrent restore operations
    restore_in_progress: AtomicBool,
}

impl StateManager {
    /// Create a new state manager with custom storage backend
    pub fn new_with_storage(
        state_file: PathBuf,
        registry: Arc<Registry>,
        tei_binary_path: String,
        storage: Arc<dyn StorageBackend>,
    ) -> Self {
        Self {
            state_file,
            registry,
            tei_binary_path: Arc::from(tei_binary_path),
            storage,
            restore_in_progress: AtomicBool::new(false),
        }
    }

    /// Create a new state manager with default filesystem storage
    pub fn new(state_file: PathBuf, registry: Arc<Registry>, tei_binary_path: String) -> Self {
        Self::new_with_storage(
            state_file,
            registry,
            tei_binary_path,
            Arc::new(FileSystemStorage::new()),
        )
    }

    /// Save current state to disk atomically
    pub async fn save(&self) -> Result<()> {
        let instances = self.registry.list().await;

        let state = SavedState {
            last_updated: chrono::Utc::now(),
            instances: instances.iter().map(|i| i.config.clone()).collect(),
        };

        let toml_content =
            toml::to_string_pretty(&state).context("Failed to serialize state to TOML")?;

        self.storage.save(&self.state_file, &toml_content).await?;

        tracing::debug!(
            path = ?self.state_file,
            instances = state.instances.len(),
            "State saved"
        );

        Ok(())
    }

    /// Load state from disk
    /// FAILS HARD if state file is corrupted - user must fix or delete
    pub async fn load(&self) -> Result<SavedState> {
        let content = self.storage.load(&self.state_file).await?;

        let content = match content {
            Some(c) => c,
            None => {
                tracing::info!("No state file found, starting fresh");
                return Ok(SavedState::default());
            }
        };

        let state: SavedState = toml::from_str(&content).with_context(|| {
            format!(
                "Failed to parse state file: {:?}. File may be corrupted. \
                Please delete or fix the file manually.",
                self.state_file
            )
        })?;

        tracing::info!(
            instances = state.instances.len(),
            last_updated = %state.last_updated,
            "State loaded from disk"
        );

        Ok(state)
    }

    /// Restore instances from saved state
    ///
    /// This function is guarded against concurrent execution. If a restore is already
    /// in progress, this call will return an error rather than starting a new restore
    /// that could conflict with the in-flight operations.
    ///
    /// Spawned readiness-check tasks are tracked via JoinSet and awaited before
    /// returning, ensuring the restore operation is fully complete.
    ///
    /// Set `wait_for_ready` to false to skip waiting for instances to become ready
    /// (useful for tests where mock instances don't respond to health checks).
    pub async fn restore(&self) -> Result<()> {
        self.restore_with_options(true).await
    }

    /// Restore instances with configurable readiness wait
    pub async fn restore_with_options(&self, wait_for_ready: bool) -> Result<()> {
        // Attempt to acquire the restore guard
        if self
            .restore_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            anyhow::bail!("Restore operation already in progress");
        }

        // Ensure we release the guard on exit (success or failure)
        let _guard = RestoreGuard {
            flag: &self.restore_in_progress,
        };

        let state = self.load().await?;

        if state.instances.is_empty() {
            tracing::info!("No instances to restore");
            return Ok(());
        }

        tracing::info!(
            instances = state.instances.len(),
            "Restoring instances from state"
        );

        let mut restored = 0;
        let mut failed = 0;
        let mut readiness_tasks: JoinSet<(String, Result<(), anyhow::Error>)> = JoinSet::new();

        for config in state.instances {
            match self.registry.add(config.clone()).await {
                Ok(instance) => {
                    if let Err(e) = instance.start(&self.tei_binary_path).await {
                        tracing::error!(
                            instance = %config.name,
                            error = %e,
                            "Failed to start restored instance"
                        );
                        failed += 1;
                    } else {
                        if wait_for_ready {
                            // Track background task for readiness check
                            let instance_clone = instance.clone();
                            let instance_name = config.name.clone();
                            readiness_tasks.spawn(async move {
                                use crate::health::GrpcHealthChecker;
                                use std::time::Duration;

                                let result = GrpcHealthChecker::wait_for_ready(
                                    &instance_clone,
                                    Duration::from_secs(300),
                                    Duration::from_millis(500),
                                )
                                .await;

                                if let Err(ref e) = result {
                                    tracing::error!(
                                        instance = %instance_clone.config.name,
                                        error = %e,
                                        "Restored instance failed to become ready"
                                    );
                                    *instance_clone.status.write().await =
                                        crate::instance::InstanceStatus::Failed;
                                }

                                (instance_name, result)
                            });
                        }
                        restored += 1;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        instance = %config.name,
                        error = %e,
                        "Failed to restore instance"
                    );
                    failed += 1;
                }
            }
        }

        // Wait for all readiness checks to complete
        let mut readiness_failed = 0;
        while let Some(result) = readiness_tasks.join_next().await {
            match result {
                Ok((name, Ok(()))) => {
                    tracing::debug!(instance = %name, "Instance readiness check completed");
                }
                Ok((name, Err(_))) => {
                    tracing::warn!(instance = %name, "Instance readiness check failed");
                    readiness_failed += 1;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Readiness task panicked");
                    readiness_failed += 1;
                }
            }
        }

        tracing::info!(
            restored = restored,
            failed = failed,
            readiness_failed = readiness_failed,
            "Instance restoration complete"
        );

        Ok(())
    }
}

/// RAII guard to ensure restore_in_progress flag is cleared on drop
struct RestoreGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SavedState {
    pub last_updated: chrono::DateTime<chrono::Utc>,
    pub instances: Vec<InstanceConfig>,
}

// ============================================================================
// Mock Implementation for Testing
// ============================================================================

#[cfg(test)]
pub mod mocks {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    /// Mock storage backend for testing
    pub struct MockStorage {
        files: Arc<RwLock<HashMap<PathBuf, String>>>,
        save_error: Arc<RwLock<Option<String>>>,
        load_error: Arc<RwLock<Option<String>>>,
    }

    impl Default for MockStorage {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockStorage {
        pub fn new() -> Self {
            Self {
                files: Arc::new(RwLock::new(HashMap::new())),
                save_error: Arc::new(RwLock::new(None)),
                load_error: Arc::new(RwLock::new(None)),
            }
        }

        /// Get the content of a file
        pub async fn get_file(&self, path: &Path) -> Option<String> {
            self.files.read().await.get(path).cloned()
        }

        /// Check how many files are stored
        pub async fn file_count(&self) -> usize {
            self.files.read().await.len()
        }

        /// Clear all files
        pub async fn clear(&self) {
            self.files.write().await.clear();
        }

        /// Set an error to return on next save
        pub async fn set_save_error(&self, error: String) {
            *self.save_error.write().await = Some(error);
        }

        /// Set an error to return on next load
        pub async fn set_load_error(&self, error: String) {
            *self.load_error.write().await = Some(error);
        }

        /// Verify atomic write behavior (temp file not left behind)
        pub async fn has_temp_file(&self, base_path: &Path) -> bool {
            let temp_path = base_path.with_extension("tmp");
            self.files.read().await.contains_key(&temp_path)
        }
    }

    #[async_trait]
    impl StorageBackend for MockStorage {
        async fn save(&self, path: &Path, content: &str) -> Result<()> {
            // Check for error injection
            if let Some(error) = self.save_error.write().await.take() {
                return Err(anyhow::anyhow!(error));
            }

            // Simulate atomic write
            let temp_path = path.with_extension("tmp");
            self.files
                .write()
                .await
                .insert(temp_path.clone(), content.to_string());

            // "Rename" - remove temp, add final
            self.files.write().await.remove(&temp_path);
            self.files
                .write()
                .await
                .insert(path.to_path_buf(), content.to_string());

            Ok(())
        }

        async fn load(&self, path: &Path) -> Result<Option<String>> {
            // Check for error injection
            if let Some(error) = self.load_error.write().await.take() {
                return Err(anyhow::anyhow!(error));
            }

            Ok(self.files.read().await.get(path).cloned())
        }

        fn exists(&self, path: &Path) -> bool {
            // For synchronous check, we can't use async RwLock
            // In tests, we'll use the async version through the trait
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async { self.files.read().await.contains_key(path) })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mocks::MockStorage;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_save_and_load_with_mock() {
        let state_file = PathBuf::from("/test/state.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file.clone(),
            registry.clone(),
            "text-embeddings-router".to_string(),
            storage.clone(),
        );

        // Add an instance
        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            gpu_id: Some(1),
            created_at: Some(chrono::Utc::now()),
            ..Default::default()
        };

        registry.add(config.clone()).await.unwrap();

        // Save state
        state_manager.save().await.unwrap();

        // Verify file was saved
        assert_eq!(storage.file_count().await, 1);
        assert!(storage.get_file(&state_file).await.is_some());

        // Load state
        let loaded = state_manager.load().await.unwrap();
        assert_eq!(loaded.instances.len(), 1);
        assert_eq!(loaded.instances[0].name, "test");
        assert_eq!(loaded.instances[0].gpu_id, Some(1));
    }

    #[tokio::test]
    async fn test_load_nonexistent_file() {
        let state_file = PathBuf::from("/test/nonexistent.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry,
            "text-embeddings-router".to_string(),
            storage,
        );

        // Loading nonexistent file should return default state
        let loaded = state_manager.load().await.unwrap();
        assert_eq!(loaded.instances.len(), 0);
    }

    #[tokio::test]
    async fn test_corrupted_state_fails() {
        let state_file = PathBuf::from("/test/corrupted.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        // Manually insert corrupted TOML
        storage
            .save(&state_file, "this is not valid TOML {{{}}")
            .await
            .unwrap();

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry,
            "text-embeddings-router".to_string(),
            storage,
        );

        // Should fail hard
        assert!(state_manager.load().await.is_err());
    }

    #[tokio::test]
    async fn test_save_multiple_instances() {
        let state_file = PathBuf::from("/test/multi.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file.clone(),
            registry.clone(),
            "text-embeddings-router".to_string(),
            storage.clone(),
        );

        // Add multiple instances
        for i in 0..3 {
            let config = InstanceConfig {
                name: format!("inst{}", i),
                model_id: format!("model{}", i),
                port: 8080 + i as u16,
                gpu_id: Some(i),
                created_at: Some(chrono::Utc::now()),
                ..Default::default()
            };
            registry.add(config).await.unwrap();
        }

        state_manager.save().await.unwrap();

        let loaded = state_manager.load().await.unwrap();
        assert_eq!(loaded.instances.len(), 3);
    }

    #[tokio::test]
    async fn test_save_error_handling() {
        let state_file = PathBuf::from("/test/error.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry.clone(),
            "text-embeddings-router".to_string(),
            storage.clone(),
        );

        // Add an instance
        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            created_at: Some(chrono::Utc::now()),
            ..Default::default()
        };
        registry.add(config).await.unwrap();

        // Inject save error
        storage.set_save_error("Disk full".to_string()).await;

        // Save should fail
        assert!(state_manager.save().await.is_err());
    }

    #[tokio::test]
    async fn test_load_error_handling() {
        let state_file = PathBuf::from("/test/load_error.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry,
            "text-embeddings-router".to_string(),
            storage.clone(),
        );

        // Inject load error
        storage
            .set_load_error("Permission denied".to_string())
            .await;

        // Load should fail
        assert!(state_manager.load().await.is_err());
    }

    #[tokio::test]
    async fn test_atomic_write_no_temp_files() {
        let state_file = PathBuf::from("/test/atomic.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file.clone(),
            registry.clone(),
            "text-embeddings-router".to_string(),
            storage.clone(),
        );

        // Add instance and save
        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            created_at: Some(chrono::Utc::now()),
            ..Default::default()
        };
        registry.add(config).await.unwrap();
        state_manager.save().await.unwrap();

        // Temp file should not exist after successful save
        assert!(!storage.has_temp_file(&state_file).await);
    }

    #[tokio::test]
    async fn test_save_empty_registry() {
        let state_file = PathBuf::from("/test/empty.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file.clone(),
            registry,
            "text-embeddings-router".to_string(),
            storage.clone(),
        );

        // Save with no instances
        state_manager.save().await.unwrap();

        // Verify file was saved
        let content = storage.get_file(&state_file).await.unwrap();
        assert!(content.contains("instances = []"));
    }

    #[tokio::test]
    async fn test_toml_serialization_format() {
        let state_file = PathBuf::from("/test/format.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file.clone(),
            registry.clone(),
            "text-embeddings-router".to_string(),
            storage.clone(),
        );

        // Add instance with specific values
        let config = InstanceConfig {
            name: "test-instance".to_string(),
            model_id: "bert-base".to_string(),
            port: 9090,
            max_batch_tokens: 2048,
            max_concurrent_requests: 20,
            pooling: Some("mean".to_string()),
            gpu_id: Some(1),
            prometheus_port: Some(9091),
            extra_args: vec!["--arg1".to_string()],
            created_at: Some(chrono::Utc::now()),
            ..Default::default()
        };
        registry.add(config).await.unwrap();

        state_manager.save().await.unwrap();

        // Verify TOML content
        let content = storage.get_file(&state_file).await.unwrap();
        assert!(content.contains("name = \"test-instance\""));
        assert!(content.contains("model_id = \"bert-base\""));
        assert!(content.contains("port = 9090"));
        assert!(content.contains("pooling = \"mean\""));
    }

    #[tokio::test]
    async fn test_filesystem_storage_integration() {
        let temp_dir = TempDir::new().unwrap();
        let state_file = temp_dir.path().join("state.toml");

        let storage = Arc::new(FileSystemStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file.clone(),
            registry.clone(),
            "text-embeddings-router".to_string(),
            storage,
        );

        // Add instance
        let config = InstanceConfig {
            name: "fs-test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            created_at: Some(chrono::Utc::now()),
            ..Default::default()
        };
        registry.add(config).await.unwrap();

        // Save to real filesystem
        state_manager.save().await.unwrap();

        // Verify file exists
        assert!(state_file.exists());

        // Load from real filesystem
        let loaded = state_manager.load().await.unwrap();
        assert_eq!(loaded.instances.len(), 1);
        assert_eq!(loaded.instances[0].name, "fs-test");
    }

    #[tokio::test]
    async fn test_concurrent_restore_prevented() {
        use std::sync::atomic::Ordering;

        let state_file = PathBuf::from("/test/concurrent.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry,
            "text-embeddings-router".to_string(),
            storage,
        );

        // Simulate a restore already in progress by setting the flag
        state_manager
            .restore_in_progress
            .store(true, Ordering::SeqCst);

        // Attempting another restore should fail
        let result = state_manager.restore().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already in progress")
        );

        // Reset the flag
        state_manager
            .restore_in_progress
            .store(false, Ordering::SeqCst);

        // Now restore should work (with empty state)
        let result = state_manager.restore().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_restore_guard_cleared_on_completion() {
        use std::sync::atomic::Ordering;

        let state_file = PathBuf::from("/test/guard.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry,
            "text-embeddings-router".to_string(),
            storage,
        );

        // Flag should start as false
        assert!(!state_manager.restore_in_progress.load(Ordering::SeqCst));

        // Run restore (with empty state)
        state_manager.restore().await.unwrap();

        // Flag should be cleared after restore completes
        assert!(!state_manager.restore_in_progress.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_restore_guard_cleared_on_error() {
        use std::sync::atomic::Ordering;

        let state_file = PathBuf::from("/test/guard_error.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        // Inject load error to make restore fail
        storage
            .set_load_error("Simulated IO error".to_string())
            .await;

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry,
            "text-embeddings-router".to_string(),
            storage,
        );

        // Flag should start as false
        assert!(!state_manager.restore_in_progress.load(Ordering::SeqCst));

        // Run restore (should fail due to load error)
        let result = state_manager.restore().await;
        assert!(result.is_err());

        // Flag should still be cleared after restore fails (RAII guard)
        assert!(!state_manager.restore_in_progress.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_restore_with_registry_add_failure() {
        let state_file = PathBuf::from("/test/add_failure.toml");
        let storage = Arc::new(MockStorage::new());

        // Create registry with max_instances = 1
        let registry = Arc::new(Registry::new(
            Some(1),
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        // Create state with 2 instances (second will fail due to limit)
        let state_content = r#"
last_updated = "2025-01-01T00:00:00Z"

[[instances]]
name = "instance1"
model_id = "model1"
port = 8080
max_batch_tokens = 1024
max_concurrent_requests = 10

[[instances]]
name = "instance2"
model_id = "model2"
port = 8081
max_batch_tokens = 1024
max_concurrent_requests = 10
"#;

        storage.save(&state_file, state_content).await.unwrap();

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry.clone(),
            "text-embeddings-router".to_string(),
            storage,
        );

        // Restore should complete (not panic) even though second instance fails
        let result = state_manager.restore_with_options(false).await;
        assert!(result.is_ok());

        // Only 1 instance should be in registry
        let instances = registry.list().await;
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].config.name, "instance1");
    }

    #[tokio::test]
    async fn test_restore_with_duplicate_port_failure() {
        let state_file = PathBuf::from("/test/dup_port.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        // Create state with 2 instances using same port (second will fail)
        let state_content = r#"
last_updated = "2025-01-01T00:00:00Z"

[[instances]]
name = "instance1"
model_id = "model1"
port = 8080
max_batch_tokens = 1024
max_concurrent_requests = 10

[[instances]]
name = "instance2"
model_id = "model2"
port = 8080
max_batch_tokens = 1024
max_concurrent_requests = 10
"#;

        storage.save(&state_file, state_content).await.unwrap();

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry.clone(),
            "text-embeddings-router".to_string(),
            storage,
        );

        // Restore should complete even though second instance fails (port conflict)
        let result = state_manager.restore_with_options(false).await;
        assert!(result.is_ok());

        // Only 1 instance should be in registry
        let instances = registry.list().await;
        assert_eq!(instances.len(), 1);
    }

    #[tokio::test]
    async fn test_restore_without_waiting_for_ready() {
        let state_file = PathBuf::from("/test/no_wait.toml");
        let storage = Arc::new(MockStorage::new());
        let registry = Arc::new(Registry::new(
            None,
            "/bin/sleep".to_string(), // Stub binary
            8080,
            8180,
        ));

        let state_content = r#"
last_updated = "2025-01-01T00:00:00Z"

[[instances]]
name = "no-wait-instance"
model_id = "model"
port = 8080
max_batch_tokens = 1024
max_concurrent_requests = 10
"#;

        storage.save(&state_file, state_content).await.unwrap();

        let state_manager = StateManager::new_with_storage(
            state_file,
            registry.clone(),
            "/bin/sleep".to_string(),
            storage,
        );

        // Restore with wait_for_ready=false should complete quickly
        let result = state_manager.restore_with_options(false).await;
        assert!(result.is_ok());

        // Instance should be in registry
        let instances = registry.list().await;
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].config.name, "no-wait-instance");
    }
}
