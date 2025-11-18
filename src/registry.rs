//! Thread-safe instance registry

use crate::config::InstanceConfig;
use crate::instance::TeiInstance;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Thread-safe registry for managing TEI instances
pub struct Registry {
    instances: Arc<RwLock<HashMap<String, Arc<TeiInstance>>>>,
    max_instances: Option<usize>,
}

impl Registry {
    /// Create a new registry
    pub fn new(max_instances: Option<usize>) -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            max_instances,
        }
    }

    /// Add a new instance to the registry
    /// Returns error if name exists, port conflicts, or max instances reached
    pub async fn add(&self, config: InstanceConfig) -> Result<Arc<TeiInstance>> {
        let mut instances = self.instances.write().await;

        // Validate uniqueness
        if instances.contains_key(&config.name) {
            anyhow::bail!("Instance '{}' already exists", config.name);
        }

        // Check port conflicts
        for instance in instances.values() {
            if instance.config.port == config.port {
                anyhow::bail!(
                    "Port {} already in use by instance '{}'",
                    config.port,
                    instance.config.name
                );
            }
        }

        // Check max instances
        if let Some(max) = self.max_instances
            && instances.len() >= max
        {
            anyhow::bail!("Maximum instance count ({}) reached", max);
        }

        let instance = Arc::new(TeiInstance::new(config));
        instances.insert(instance.config.name.clone(), instance.clone());

        tracing::info!(
            instance = %instance.config.name,
            total_instances = instances.len(),
            "Instance added to registry"
        );

        Ok(instance)
    }

    /// Get instance by name
    pub async fn get(&self, name: &str) -> Option<Arc<TeiInstance>> {
        let instances = self.instances.read().await;
        instances.get(name).cloned()
    }

    /// Remove instance and stop it
    pub async fn remove(&self, name: &str) -> Result<()> {
        let mut instances = self.instances.write().await;

        let instance = instances
            .remove(name)
            .with_context(|| format!("Instance '{}' not found", name))?;

        // Drop write lock before stopping (stop may take time)
        drop(instances);

        instance.stop().await?;

        tracing::info!(instance = %name, "Instance removed from registry");

        Ok(())
    }

    /// List all instances
    pub async fn list(&self) -> Vec<Arc<TeiInstance>> {
        let instances = self.instances.read().await;
        instances.values().cloned().collect()
    }

    /// Get instance count
    pub async fn count(&self) -> usize {
        let instances = self.instances.read().await;
        instances.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_registry_add_and_get() {
        let registry = Registry::new(None);

        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            extra_args: vec![],
            created_at: None,
        };

        let instance = registry.add(config).await.unwrap();
        assert_eq!(instance.config.name, "test");
        assert_eq!(registry.count().await, 1);

        let retrieved = registry.get("test").await.unwrap();
        assert_eq!(retrieved.config.name, "test");
    }

    #[tokio::test]
    async fn test_duplicate_name_rejection() {
        let registry = Registry::new(None);

        let config1 = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            extra_args: vec![],
            created_at: None,
        };

        let config2 = InstanceConfig {
            name: "test".to_string(), // Same name
            model_id: "model2".to_string(),
            port: 8081,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            extra_args: vec![],
            created_at: None,
        };

        registry.add(config1).await.unwrap();
        assert!(registry.add(config2).await.is_err());
    }

    #[tokio::test]
    async fn test_port_conflict_detection() {
        let registry = Registry::new(None);

        let config1 = InstanceConfig {
            name: "test1".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            extra_args: vec![],
            created_at: None,
        };

        let config2 = InstanceConfig {
            name: "test2".to_string(),
            model_id: "model2".to_string(),
            port: 8080, // Same port
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            extra_args: vec![],
            created_at: None,
        };

        registry.add(config1).await.unwrap();
        assert!(registry.add(config2).await.is_err());
    }

    #[tokio::test]
    async fn test_max_instances_limit() {
        let registry = Registry::new(Some(2));

        for i in 0..2 {
            let config = InstanceConfig {
                name: format!("test{}", i),
                model_id: "model".to_string(),
                port: 8080 + i as u16,
                max_batch_tokens: 1024,
                max_concurrent_requests: 10,
                pooling: None,
                gpu_id: None,
                extra_args: vec![],
                created_at: None,
            };
            registry.add(config).await.unwrap();
        }

        // Third should fail
        let config3 = InstanceConfig {
            name: "test3".to_string(),
            model_id: "model".to_string(),
            port: 8082,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            extra_args: vec![],
            created_at: None,
        };

        assert!(registry.add(config3).await.is_err());
    }
}
