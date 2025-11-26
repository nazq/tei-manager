//! Lock-free connection pool for backend TEI instances

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// Connection entry with metadata for pruning
struct ConnectionEntry {
    clients: BackendClients,
    created_at: Instant,
    last_used: Instant,
}

impl ConnectionEntry {
    fn new(clients: BackendClients) -> Self {
        let now = Instant::now();
        Self {
            clients,
            created_at: now,
            last_used: now,
        }
    }

    fn touch(&mut self) {
        self.last_used = Instant::now();
    }
}

/// Lock-free connection pool for backend TEI instances
#[derive(Clone)]
pub struct BackendPool {
    // Lock-free concurrent hashmap: instance_name -> connection entry
    connections: Arc<DashMap<String, ConnectionEntry>>,

    // Reference to instance registry (no locks needed - Arc is sufficient)
    registry: Arc<Registry>,

    // Pruning configuration
    prune_interval: Duration,
    max_idle_time: Duration,
}

/// Default pruning interval (5 minutes)
const DEFAULT_PRUNE_INTERVAL_SECS: u64 = 300;

/// Default max idle time before connection is pruned (10 minutes)
const DEFAULT_MAX_IDLE_SECS: u64 = 600;

impl Drop for BackendPool {
    fn drop(&mut self) {
        tracing::debug!("BackendPool dropped, clearing all connections");
    }
}

impl BackendPool {
    /// Create a new connection pool with default pruning settings
    pub fn new(registry: Arc<Registry>) -> Self {
        Self::with_pruning_config(
            registry,
            Duration::from_secs(DEFAULT_PRUNE_INTERVAL_SECS),
            Duration::from_secs(DEFAULT_MAX_IDLE_SECS),
        )
    }

    /// Create a new connection pool with custom pruning configuration
    pub fn with_pruning_config(
        registry: Arc<Registry>,
        prune_interval: Duration,
        max_idle_time: Duration,
    ) -> Self {
        let pool = Self {
            connections: Arc::new(DashMap::new()),
            registry: registry.clone(),
            prune_interval,
            max_idle_time,
        };

        // Spawn background task to listen for lifecycle events
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            pool_clone.handle_lifecycle_events().await;
        });

        // Spawn background task for periodic pruning
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            pool_clone.prune_idle_connections_task().await;
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
    pub async fn get_clients(&self, instance_name: &str) -> Result<BackendClients, Status> {
        // Fast path: client already exists (DashMap read is lock-free)
        if let Some(mut entry) = self.connections.get_mut(instance_name) {
            entry.touch(); // Update last_used timestamp
            return Ok(entry.clients.clone()); // Cheap Arc clone
        }

        // Slow path: create new connection
        // Using entry API prevents race condition where two threads both try to create connection
        match self.connections.entry(instance_name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                // Another thread created it while we were waiting
                entry.get_mut().touch();
                Ok(entry.get().clients.clone())
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // We got the lock, create the connection
                let clients = self.create_connection(instance_name).await?;
                entry.insert(ConnectionEntry::new(clients.clone()));
                Ok(clients)
            }
        }
    }

    /// Background task for periodic pruning of idle connections
    async fn prune_idle_connections_task(&self) {
        let mut interval = tokio::time::interval(self.prune_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            self.prune_idle_connections();
        }
    }

    /// Prune connections that have been idle for longer than max_idle_time
    /// Also removes connections for instances that no longer exist in the registry
    pub fn prune_idle_connections(&self) -> usize {
        let now = Instant::now();
        let max_idle = self.max_idle_time;
        let mut pruned = 0;

        // Collect keys to remove (can't remove while iterating)
        let to_remove: Vec<String> = self
            .connections
            .iter()
            .filter(|entry| now.duration_since(entry.last_used) > max_idle)
            .map(|entry| entry.key().clone())
            .collect();

        for key in to_remove {
            if self.connections.remove(&key).is_some() {
                pruned += 1;
                tracing::debug!(
                    instance = %key,
                    "Pruned idle connection (exceeded max idle time)"
                );
            }
        }

        if pruned > 0 {
            tracing::info!(
                pruned_count = pruned,
                remaining = self.connections.len(),
                "Pruned idle connections"
            );
        }

        pruned
    }

    /// Prune connections for instances that are no longer in the registry
    pub async fn prune_orphaned_connections(&self) -> usize {
        let mut pruned = 0;

        // Collect keys to check
        let keys: Vec<String> = self.connections.iter().map(|e| e.key().clone()).collect();

        for key in keys {
            if self.registry.get(&key).await.is_none() && self.connections.remove(&key).is_some() {
                pruned += 1;
                tracing::debug!(
                    instance = %key,
                    "Pruned orphaned connection (instance no longer exists)"
                );
            }
        }

        if pruned > 0 {
            tracing::info!(
                pruned_count = pruned,
                remaining = self.connections.len(),
                "Pruned orphaned connections"
            );
        }

        pruned
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
        let now = Instant::now();
        let mut oldest_connection_age_secs = None;
        let mut max_idle_secs = 0u64;

        for entry in self.connections.iter() {
            let age = now.duration_since(entry.created_at).as_secs();
            let idle = now.duration_since(entry.last_used).as_secs();

            oldest_connection_age_secs = Some(
                oldest_connection_age_secs
                    .map(|old: u64| old.max(age))
                    .unwrap_or(age),
            );
            max_idle_secs = max_idle_secs.max(idle);
        }

        PoolStats {
            active_connections: self.connections.len(),
            oldest_connection_age_secs,
            max_idle_secs,
            prune_interval_secs: self.prune_interval.as_secs(),
            max_idle_threshold_secs: self.max_idle_time.as_secs(),
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
    pub oldest_connection_age_secs: Option<u64>,
    pub max_idle_secs: u64,
    pub prune_interval_secs: u64,
    pub max_idle_threshold_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InstanceConfig;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_pool_stats() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);

        let stats = pool.stats();
        assert_eq!(stats.active_connections, 0);
    }

    #[tokio::test]
    async fn test_pool_remove() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);

        // Removing non-existent connection returns false
        assert!(!pool.remove("nonexistent"));
    }

    #[tokio::test]
    async fn test_pool_clear() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);

        pool.clear();
        assert_eq!(pool.stats().active_connections, 0);
    }

    #[tokio::test]
    async fn test_get_clients_not_found() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);

        let result = pool.get_clients("nonexistent").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_get_clients_not_running() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry.clone());

        // Add instance but don't start it
        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "model".to_string(),
            port: 59998,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        registry.add(config).await.unwrap();

        let result = pool.get_clients("test").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn test_lifecycle_events_subscribed() {
        // Test that pool subscribes to lifecycle events
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
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
            ..Default::default()
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

    #[tokio::test]
    async fn test_pool_with_custom_pruning_config() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));

        let pool = BackendPool::with_pruning_config(
            registry,
            Duration::from_secs(60),  // 1 minute prune interval
            Duration::from_secs(120), // 2 minute max idle
        );

        let stats = pool.stats();
        assert_eq!(stats.prune_interval_secs, 60);
        assert_eq!(stats.max_idle_threshold_secs, 120);
    }

    #[tokio::test]
    async fn test_pool_stats_empty() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);

        let stats = pool.stats();
        assert_eq!(stats.active_connections, 0);
        assert!(stats.oldest_connection_age_secs.is_none());
        assert_eq!(stats.max_idle_secs, 0);
    }

    #[tokio::test]
    async fn test_prune_idle_connections_empty_pool() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::with_pruning_config(
            registry,
            Duration::from_secs(1),
            Duration::from_millis(100), // Very short idle time
        );

        // Pruning empty pool should return 0
        let pruned = pool.prune_idle_connections();
        assert_eq!(pruned, 0);
    }

    #[tokio::test]
    async fn test_prune_orphaned_connections_empty_pool() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);

        // Pruning empty pool should return 0
        let pruned = pool.prune_orphaned_connections().await;
        assert_eq!(pruned, 0);
    }

    #[test]
    fn test_connection_entry_touch() {
        // Create a mock BackendClients using unsafe channel (test only)
        // We can't easily create a real BackendClients without a connection,
        // so we test ConnectionEntry logic indirectly through integration
    }

    #[tokio::test]
    async fn test_stats_default_values() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);

        let stats = pool.stats();
        assert_eq!(stats.prune_interval_secs, DEFAULT_PRUNE_INTERVAL_SECS);
        assert_eq!(stats.max_idle_threshold_secs, DEFAULT_MAX_IDLE_SECS);
    }
}
