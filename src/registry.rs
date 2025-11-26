//! Thread-safe instance registry

use crate::config::InstanceConfig;
use crate::instance::TeiInstance;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

/// Events that occur during instance lifecycle
#[derive(Debug, Clone)]
pub enum InstanceEvent {
    /// Instance was added to registry
    Added(String),
    /// Instance was removed from registry
    Removed(String),
    /// Instance was started
    Started(String),
    /// Instance was stopped
    Stopped(String),
}

/// Thread-safe registry for managing TEI instances
pub struct Registry {
    instances: Arc<RwLock<HashMap<String, Arc<TeiInstance>>>>,
    max_instances: Option<usize>,
    tei_binary_path: Arc<str>,
    next_prometheus_port: Arc<RwLock<u16>>,
    next_instance_port: Arc<RwLock<u16>>,
    /// Port range for auto-allocation [start, end)
    /// If start == end, auto-allocation is disabled
    instance_port_range: (u16, u16),
    event_tx: broadcast::Sender<InstanceEvent>,
}

impl Registry {
    /// Create a new registry
    ///
    /// # Arguments
    /// * `max_instances` - Maximum number of instances allowed
    /// * `tei_binary_path` - Path to the TEI binary
    /// * `instance_port_start` - Start of port range for auto-allocation
    /// * `instance_port_end` - End of port range for auto-allocation (exclusive)
    ///
    /// If instance_port_start == instance_port_end, auto-allocation is disabled
    pub fn new(
        max_instances: Option<usize>,
        tei_binary_path: String,
        instance_port_start: u16,
        instance_port_end: u16,
    ) -> Self {
        // Create broadcast channel for lifecycle events
        // Capacity of 100 should be sufficient for most use cases
        let (event_tx, _) = broadcast::channel(100);

        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            max_instances,
            tei_binary_path: Arc::from(tei_binary_path),
            next_prometheus_port: Arc::new(RwLock::new(9100)),
            next_instance_port: Arc::new(RwLock::new(instance_port_start)),
            instance_port_range: (instance_port_start, instance_port_end),
            event_tx,
        }
    }

    /// Subscribe to lifecycle events
    pub fn subscribe_events(&self) -> broadcast::Receiver<InstanceEvent> {
        self.event_tx.subscribe()
    }

    /// Check if port auto-allocation is enabled
    pub fn is_port_auto_allocation_enabled(&self) -> bool {
        self.instance_port_range.0 < self.instance_port_range.1
    }

    /// Add a new instance to the registry
    /// Returns error if name exists, port conflicts, or max instances reached
    ///
    /// If `config.port` is 0, auto-allocates a port from the configured range
    pub async fn add(&self, mut config: InstanceConfig) -> Result<Arc<TeiInstance>> {
        let mut instances = self.instances.write().await;

        // Validate uniqueness
        if instances.contains_key(&config.name) {
            anyhow::bail!("Instance '{}' already exists", config.name);
        }

        // Auto-assign instance port if not specified (port == 0)
        if config.port == 0 {
            if !self.is_port_auto_allocation_enabled() {
                anyhow::bail!(
                    "Port not specified and auto-allocation is disabled (no port range configured)"
                );
            }

            let mut next_port = self.next_instance_port.write().await;

            // Collect used ports
            let used_ports: std::collections::HashSet<u16> =
                instances.values().map(|i| i.config.port).collect();

            // Find next available port in range, starting from next_port
            // If next_port is past the end of the range, wrap around to start
            let search_start = if *next_port >= self.instance_port_range.1 {
                self.instance_port_range.0
            } else {
                *next_port
            };

            let assigned_port = Self::find_free_port_in_range(
                search_start,
                self.instance_port_range.0,
                self.instance_port_range.1,
                &used_ports,
            )?;
            config.port = assigned_port;

            // Update next_port for next allocation
            *next_port = assigned_port + 1;

            tracing::info!(port = assigned_port, "Auto-assigned instance port");
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

        // Auto-assign Prometheus port if not specified
        if config.prometheus_port.is_none() {
            let mut next_port = self.next_prometheus_port.write().await;

            // Find next available port starting from current next_port
            let assigned_port = Self::find_free_port(*next_port)?;
            config.prometheus_port = Some(assigned_port);

            // Update next_port for next allocation
            *next_port = assigned_port + 1;
        }

        let instance = Arc::new(TeiInstance::new(config));
        let instance_name = instance.config.name.clone();

        tracing::info!(
            instance = %instance_name,
            total_instances = instances.len() + 1,
            prometheus_port = ?instance.config.prometheus_port,
            "Instance added to registry"
        );

        instances.insert(instance_name.clone(), instance.clone());

        // Notify listeners of the add event
        let _ = self.event_tx.send(InstanceEvent::Added(instance_name));

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

        // Notify listeners of the removal
        let _ = self.event_tx.send(InstanceEvent::Removed(name.to_string()));

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

    /// Get TEI binary path
    pub fn tei_binary_path(&self) -> &str {
        &self.tei_binary_path
    }

    /// Find next available port starting from the given port
    /// Tries up to 1000 ports to find a free one
    fn find_free_port(start_port: u16) -> Result<u16> {
        const MAX_ATTEMPTS: u16 = 1000;

        for offset in 0..MAX_ATTEMPTS {
            let port = start_port.saturating_add(offset);

            // Try to bind to the port to check if it's free
            if TcpListener::bind(("0.0.0.0", port)).is_ok() {
                return Ok(port);
            }
        }

        anyhow::bail!(
            "Could not find free port in range {}-{}",
            start_port,
            start_port.saturating_add(MAX_ATTEMPTS)
        )
    }

    /// Find next available port in a given range, avoiding already-used ports
    /// Searches from search_start to range_end, then wraps around from range_start
    fn find_free_port_in_range(
        search_start: u16,
        range_start: u16,
        range_end: u16,
        used_ports: &std::collections::HashSet<u16>,
    ) -> Result<u16> {
        // Search from search_start to range_end
        for port in search_start..range_end {
            if !used_ports.contains(&port) && TcpListener::bind(("0.0.0.0", port)).is_ok() {
                return Ok(port);
            }
        }

        // Wrap around: search from range_start to search_start
        for port in range_start..search_start {
            if !used_ports.contains(&port) && TcpListener::bind(("0.0.0.0", port)).is_ok() {
                return Ok(port);
            }
        }

        anyhow::bail!(
            "Could not find free port in range [{}, {})",
            range_start,
            range_end
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find N consecutive free ports starting from a base port.
    /// Returns the start of the range if found.
    fn find_consecutive_free_ports(start: u16, count: u16) -> Option<u16> {
        for base in start..60000 {
            let mut all_free = true;
            for offset in 0..count {
                // Use 0.0.0.0 to match production code in find_free_port_in_range
                if TcpListener::bind(("0.0.0.0", base + offset)).is_err() {
                    all_free = false;
                    break;
                }
            }
            if all_free {
                return Some(base);
            }
        }
        None
    }

    #[tokio::test]
    async fn test_registry_add_and_get() {
        let registry = Registry::new(None, "text-embeddings-router".to_string(), 8080, 8180);

        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let instance = registry.add(config).await.unwrap();
        assert_eq!(instance.config.name, "test");
        assert_eq!(registry.count().await, 1);

        let retrieved = registry.get("test").await.unwrap();
        assert_eq!(retrieved.config.name, "test");
    }

    #[tokio::test]
    async fn test_duplicate_name_rejection() {
        let registry = Registry::new(None, "text-embeddings-router".to_string(), 8080, 8180);

        let config1 = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let config2 = InstanceConfig {
            name: "test".to_string(), // Same name
            model_id: "model2".to_string(),
            port: 8081,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        registry.add(config1).await.unwrap();
        assert!(registry.add(config2).await.is_err());
    }

    #[tokio::test]
    async fn test_port_conflict_detection() {
        let registry = Registry::new(None, "text-embeddings-router".to_string(), 8080, 8180);

        let config1 = InstanceConfig {
            name: "test1".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let config2 = InstanceConfig {
            name: "test2".to_string(),
            model_id: "model2".to_string(),
            port: 8080, // Same port
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        registry.add(config1).await.unwrap();
        assert!(registry.add(config2).await.is_err());
    }

    #[tokio::test]
    async fn test_max_instances_limit() {
        let registry = Registry::new(Some(2), "text-embeddings-router".to_string(), 8080, 8180);

        for i in 0..2 {
            let config = InstanceConfig {
                name: format!("test{}", i),
                model_id: "model".to_string(),
                port: 8080 + i as u16,
                max_batch_tokens: 1024,
                max_concurrent_requests: 10,
                pooling: None,
                gpu_id: None,
                prometheus_port: None,
                ..Default::default()
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
            prometheus_port: None,
            ..Default::default()
        };

        assert!(registry.add(config3).await.is_err());
    }

    #[tokio::test]
    async fn test_port_auto_allocation_basic() {
        let registry = Registry::new(None, "text-embeddings-router".to_string(), 8080, 8180);

        // Create instance without specifying port (port = 0)
        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 0, // Auto-allocate
            ..Default::default()
        };

        let instance = registry.add(config).await.unwrap();
        assert!(instance.config.port >= 8080 && instance.config.port < 8180);
    }

    #[tokio::test]
    async fn test_port_auto_allocation_disabled() {
        // Port range with start == end disables auto-allocation
        let registry = Registry::new(None, "text-embeddings-router".to_string(), 8080, 8080);

        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 0, // Try to auto-allocate
            ..Default::default()
        };

        // Should fail since auto-allocation is disabled
        let result = registry.add(config).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("auto-allocation is disabled"));
    }

    #[tokio::test]
    async fn test_port_auto_allocation_sequential() {
        let registry = Registry::new(None, "text-embeddings-router".to_string(), 8080, 8180);

        // Create 5 instances with auto-allocated ports
        let mut ports = Vec::new();
        for i in 0..5 {
            let config = InstanceConfig {
                name: format!("test{}", i),
                model_id: "model".to_string(),
                port: 0, // Auto-allocate
                ..Default::default()
            };

            let instance = registry.add(config).await.unwrap();
            ports.push(instance.config.port);
        }

        // All ports should be unique
        let unique_ports: std::collections::HashSet<_> = ports.iter().collect();
        assert_eq!(unique_ports.len(), 5);

        // All ports should be in range
        for port in &ports {
            assert!(*port >= 8080 && *port < 8180);
        }
    }

    #[tokio::test]
    async fn test_port_auto_allocation_create_delete_create() {
        // Use a wide range so we can always find 5 free ports
        let registry = Registry::new(None, "text-embeddings-router".to_string(), 18080, 18180);

        // Create 5 instances
        for i in 0..5 {
            let config = InstanceConfig {
                name: format!("test{}", i),
                model_id: "model".to_string(),
                port: 0, // Auto-allocate
                ..Default::default()
            };
            registry.add(config).await.unwrap();
        }

        assert_eq!(registry.count().await, 5);

        // Delete 3 instances
        registry.remove("test1").await.unwrap();
        registry.remove("test2").await.unwrap();
        registry.remove("test3").await.unwrap();

        assert_eq!(registry.count().await, 2);

        // Create 3 more instances - should reuse freed ports
        for i in 5..8 {
            let config = InstanceConfig {
                name: format!("test{}", i),
                model_id: "model".to_string(),
                port: 0, // Auto-allocate
                ..Default::default()
            };
            registry.add(config).await.unwrap();
        }

        assert_eq!(registry.count().await, 5);

        // All instances should have unique ports in range
        let instances = registry.list().await;
        let ports: std::collections::HashSet<_> = instances.iter().map(|i| i.config.port).collect();
        assert_eq!(ports.len(), 5);

        for port in ports {
            assert!((18080..18180).contains(&port));
        }
    }

    #[tokio::test]
    async fn test_port_auto_allocation_exhausted() {
        // Find 2 consecutive free ports dynamically
        let base_port = find_consecutive_free_ports(19000, 2).expect("Should find 2 free ports");
        let range_end = base_port + 2;

        let registry = Registry::new(
            None,
            "text-embeddings-router".to_string(),
            base_port,
            range_end,
        );

        // Create 2 instances
        for i in 0..2 {
            let config = InstanceConfig {
                name: format!("test{}", i),
                model_id: "model".to_string(),
                port: 0, // Auto-allocate
                ..Default::default()
            };
            registry.add(config).await.unwrap();
        }

        // Third should fail - no ports available
        let config = InstanceConfig {
            name: "test_overflow".to_string(),
            model_id: "model".to_string(),
            port: 0,
            ..Default::default()
        };

        let result = registry.add(config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mixed_auto_and_manual_ports() {
        let registry = Registry::new(None, "text-embeddings-router".to_string(), 8080, 8180);

        // Create with manual port
        let config1 = InstanceConfig {
            name: "manual1".to_string(),
            model_id: "model".to_string(),
            port: 8085, // Manual port within range
            ..Default::default()
        };
        registry.add(config1).await.unwrap();

        // Create with auto port - should skip 8085
        let config2 = InstanceConfig {
            name: "auto1".to_string(),
            model_id: "model".to_string(),
            port: 0, // Auto-allocate
            ..Default::default()
        };
        let instance2 = registry.add(config2).await.unwrap();
        assert_ne!(instance2.config.port, 8085);

        // Create with manual port outside range
        let config3 = InstanceConfig {
            name: "manual2".to_string(),
            model_id: "model".to_string(),
            port: 9000, // Outside auto-allocation range
            ..Default::default()
        };
        registry.add(config3).await.unwrap();

        assert_eq!(registry.count().await, 3);
    }
}
