//! Lock-free connection pool for backend TEI instances

use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tonic::Status;
use tonic::transport::{Channel, Endpoint};

use super::proto::tei::v1::{
    embed_client::EmbedClient, info_client::InfoClient, predict_client::PredictClient,
    rerank_client::RerankClient, tokenize_client::TokenizeClient,
};
use crate::registry::Registry;

/// All gRPC clients for a single backend instance
/// Cheap to clone (all fields are Arc internally)
#[derive(Clone, Debug)]
pub struct BackendClients {
    pub embed: EmbedClient<Channel>,
    pub predict: PredictClient<Channel>,
    pub rerank: RerankClient<Channel>,
    pub tokenize: TokenizeClient<Channel>,
    pub info: InfoClient<Channel>,
}

/// Lock-free connection pool for backend TEI instances
#[derive(Clone)]
pub struct BackendPool {
    // Lock-free concurrent hashmap: instance_name -> backend clients
    connections: Arc<DashMap<String, BackendClients>>,

    // Reference to instance registry (no locks needed - Arc is sufficient)
    registry: Arc<Registry>,
}

impl Drop for BackendPool {
    fn drop(&mut self) {
        tracing::debug!("BackendPool dropped, clearing all connections");
    }
}

impl BackendPool {
    pub fn new(registry: Arc<Registry>) -> Self {
        let pool = Self {
            connections: Arc::new(DashMap::new()),
            registry: registry.clone(),
        };

        // Spawn background task to listen for lifecycle events
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            pool_clone.handle_lifecycle_events().await;
        });

        pool
    }

    /// Background task that handles instance lifecycle events
    async fn handle_lifecycle_events(&self) {
        let mut event_rx = self.registry.subscribe_events();

        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    use crate::registry::InstanceEvent;
                    match &event {
                        InstanceEvent::Removed(name) | InstanceEvent::Stopped(name) => {
                            // Remove connection when instance is removed or stopped
                            if self.remove(name) {
                                tracing::debug!(
                                    instance = %name,
                                    event = ?event,
                                    "Removed connection due to lifecycle event"
                                );
                            }
                        }
                        InstanceEvent::Added(_) | InstanceEvent::Started(_) => {
                            // No action needed - connections are created on-demand
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped = skipped,
                        "Connection pool lagged behind lifecycle events"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("Lifecycle event channel closed, stopping event handler");
                    break;
                }
            }
        }
    }

    /// Get or create clients for an instance (lock-free read, minimal locking for write)
    ///
    /// Race condition fix: Uses DashMap::entry() API to prevent duplicate connections
    pub async fn get_clients(&self, instance_name: &str) -> Result<BackendClients, Status> {
        // Fast path: client already exists (DashMap read is lock-free)
        if let Some(clients) = self.connections.get(instance_name) {
            return Ok(clients.clone()); // Cheap Arc clone
        }

        // Slow path: create new connection
        // Using entry API prevents race condition where two threads both try to create connection
        match self.connections.entry(instance_name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                // Another thread created it while we were waiting
                Ok(entry.get().clone())
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // We got the lock, create the connection
                let clients = self.create_connection(instance_name).await?;
                entry.insert(clients.clone());
                Ok(clients)
            }
        }
    }

    async fn create_connection(&self, instance_name: &str) -> Result<BackendClients, Status> {
        // Get instance info from registry
        let instance =
            self.registry.get(instance_name).await.ok_or_else(|| {
                Status::not_found(format!("Instance '{}' not found", instance_name))
            })?;

        // Note: We don't check instance status here - if the TEI server is ready,
        // we can route to it. The connection attempt below will fail naturally if not ready.

        // Build endpoint with optimized settings from TEI patterns
        let endpoint = Endpoint::from_shared(format!("http://127.0.0.1:{}", instance.config.port))
            .map_err(|e| Status::internal(format!("Invalid endpoint: {}", e)))?
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5));

        // Establish connection
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| Status::unavailable(format!("Failed to connect to backend: {}", e)))?;

        // Create all clients (they share the channel internally via HTTP/2 multiplexing)
        let clients = BackendClients {
            embed: EmbedClient::new(channel.clone()),
            predict: PredictClient::new(channel.clone()),
            rerank: RerankClient::new(channel.clone()),
            tokenize: TokenizeClient::new(channel.clone()),
            info: InfoClient::new(channel),
        };

        tracing::debug!(
            instance = instance_name,
            port = instance.config.port,
            "Created gRPC connection to backend"
        );

        Ok(clients)
    }

    /// Remove a client from the pool (when instance is deleted/stopped)
    pub fn remove(&self, instance_name: &str) -> bool {
        let removed = self.connections.remove(instance_name).is_some();
        if removed {
            tracing::debug!(
                instance = instance_name,
                "Removed backend connection from pool"
            );
        }
        removed
    }

    /// Get connection statistics
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            active_connections: self.connections.len(),
        }
    }

    /// Clear all connections (useful for testing or shutdown)
    pub fn clear(&self) {
        self.connections.clear();
        tracing::debug!("Cleared all backend connections");
    }
}

#[derive(Debug, Clone)]
pub struct PoolStats {
    pub active_connections: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InstanceConfig;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_pool_stats() {
        let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));
        let pool = BackendPool::new(registry);

        let stats = pool.stats();
        assert_eq!(stats.active_connections, 0);
    }

    #[tokio::test]
    async fn test_pool_remove() {
        let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));
        let pool = BackendPool::new(registry);

        // Removing non-existent connection returns false
        assert!(!pool.remove("nonexistent"));
    }

    #[tokio::test]
    async fn test_pool_clear() {
        let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));
        let pool = BackendPool::new(registry);

        pool.clear();
        assert_eq!(pool.stats().active_connections, 0);
    }

    #[tokio::test]
    async fn test_get_clients_not_found() {
        let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));
        let pool = BackendPool::new(registry);

        let result = pool.get_clients("nonexistent").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_get_clients_not_running() {
        let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));
        let pool = BackendPool::new(registry.clone());

        // Add instance but don't start it
        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            extra_args: vec![],
            created_at: None,
        };

        registry.add(config).await.unwrap();

        let result = pool.get_clients("test").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn test_lifecycle_events_subscribed() {
        // Test that pool subscribes to lifecycle events
        let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));
        let _pool = BackendPool::new(registry.clone());

        // Add an instance (triggers Added event)
        let config = InstanceConfig {
            name: "test-instance".to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            extra_args: vec![],
            created_at: None,
        };

        let _instance = registry.add(config).await.unwrap();

        // Subscribe to events and verify we can receive them
        let mut rx = registry.subscribe_events();

        // Remove the instance (triggers Removed event)
        registry.remove("test-instance").await.unwrap();

        // Receive the event
        tokio::select! {
            result = rx.recv() => {
                assert!(result.is_ok());
                if let Ok(event) = result {
                    matches!(event, crate::registry::InstanceEvent::Removed(_));
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                panic!("Timeout waiting for event");
            }
        }
    }
}
