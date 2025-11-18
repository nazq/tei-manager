//! State persistence for instance configurations

use crate::config::InstanceConfig;
use crate::registry::Registry;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// State manager for persisting instance configurations
pub struct StateManager {
    state_file: PathBuf,
    registry: Arc<Registry>,
}

impl StateManager {
    /// Create a new state manager
    pub fn new(state_file: PathBuf, registry: Arc<Registry>) -> Self {
        Self {
            state_file,
            registry,
        }
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

        // Atomic write: write to temp file, then rename
        let temp_file = self.state_file.with_extension("tmp");

        let mut file = fs::File::create(&temp_file)
            .await
            .context("Failed to create temp state file")?;
        file.write_all(toml_content.as_bytes())
            .await
            .context("Failed to write state file")?;
        file.sync_all().await.context("Failed to sync state file")?;

        fs::rename(&temp_file, &self.state_file)
            .await
            .context("Failed to rename temp state file")?;

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
        if !self.state_file.exists() {
            tracing::info!("No state file found, starting fresh");
            return Ok(SavedState::default());
        }

        let content = fs::read_to_string(&self.state_file)
            .await
            .with_context(|| format!("Failed to read state file: {:?}", self.state_file))?;

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
    pub async fn restore(&self) -> Result<()> {
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

        for config in state.instances {
            match self.registry.add(config.clone()).await {
                Ok(instance) => {
                    if let Err(e) = instance.start().await {
                        tracing::error!(
                            instance = %config.name,
                            error = %e,
                            "Failed to start restored instance"
                        );
                        failed += 1;
                    } else {
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

        tracing::info!(
            restored = restored,
            failed = failed,
            "Instance restoration complete"
        );

        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SavedState {
    pub last_updated: chrono::DateTime<chrono::Utc>,
    pub instances: Vec<InstanceConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let state_file = temp_dir.path().join("state.toml");

        let registry = Arc::new(Registry::new(None));
        let state_manager = StateManager::new(state_file.clone(), registry.clone());

        // Add an instance
        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: Some(1),
            extra_args: vec![],
            created_at: Some(chrono::Utc::now()),
        };

        registry.add(config.clone()).await.unwrap();

        // Save state
        state_manager.save().await.unwrap();

        // Verify file exists
        assert!(state_file.exists());

        // Load state
        let loaded = state_manager.load().await.unwrap();
        assert_eq!(loaded.instances.len(), 1);
        assert_eq!(loaded.instances[0].name, "test");
        assert_eq!(loaded.instances[0].gpu_id, Some(1));
    }

    #[tokio::test]
    async fn test_corrupted_state_fails() {
        let temp_dir = TempDir::new().unwrap();
        let state_file = temp_dir.path().join("state.toml");

        // Write corrupted TOML
        std::fs::write(&state_file, "this is not valid TOML {{{}").unwrap();

        let registry = Arc::new(Registry::new(None));
        let state_manager = StateManager::new(state_file, registry);

        // Should fail hard
        assert!(state_manager.load().await.is_err());
    }
}
