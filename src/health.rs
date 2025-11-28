//! Health monitoring for TEI instances with dependency injection and testability

use crate::instance::{InstanceStatus, TeiInstance};
use crate::registry::Registry;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::time::{Duration, interval, sleep};

// ============================================================================
// Trait Definitions
// ============================================================================

/// Result of a health check
#[derive(Debug, Clone)]
pub struct HealthCheckResult {
    pub healthy: bool,
    pub reason: Option<String>,
}

impl HealthCheckResult {
    pub fn healthy() -> Self {
        Self {
            healthy: true,
            reason: None,
        }
    }

    pub fn unhealthy(reason: String) -> Self {
        Self {
            healthy: false,
            reason: Some(reason),
        }
    }
}

/// Trait for checking instance health
#[async_trait]
pub trait HealthChecker: Send + Sync {
    async fn check(&self, instance: &TeiInstance) -> HealthCheckResult;
}

/// Trait for restarting instances
#[async_trait]
pub trait RestartStrategy: Send + Sync {
    async fn restart(&self, instance: &TeiInstance, tei_binary_path: &str) -> anyhow::Result<()>;
}

/// Events emitted by health monitor
#[derive(Debug, Clone)]
pub enum HealthEvent {
    CheckStarted {
        instance_name: String,
    },
    CheckSucceeded {
        instance_name: String,
    },
    CheckFailed {
        instance_name: String,
        consecutive_failures: u32,
        reason: String,
    },
    RestartTriggered {
        instance_name: String,
        failure_count: u32,
    },
    RestartSucceeded {
        instance_name: String,
    },
    RestartFailed {
        instance_name: String,
        error: String,
    },
    StatusTransition {
        instance_name: String,
        from: InstanceStatus,
        to: InstanceStatus,
    },
}

/// Trait for handling health events
#[async_trait]
pub trait HealthEventHandler: Send + Sync {
    async fn handle(&self, event: HealthEvent);
}

// ============================================================================
// Production Implementations
// ============================================================================

/// gRPC-based health checker that calls TEI's Info service
pub struct GrpcHealthChecker;

impl GrpcHealthChecker {
    /// Poll for instance readiness with retries after startup
    /// Returns Ok(()) when ready, Err if timeout reached
    pub async fn wait_for_ready(
        instance: &TeiInstance,
        timeout: Duration,
        poll_interval: Duration,
    ) -> anyhow::Result<()> {
        let checker = GrpcHealthChecker;
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                anyhow::bail!(
                    "Instance '{}' did not become ready within {:?}",
                    instance.config.name,
                    timeout
                );
            }

            let result = checker.check(instance).await;
            if result.healthy {
                // Update status to Running
                *instance.status.write().await = InstanceStatus::Running;
                tracing::info!(
                    instance = %instance.config.name,
                    elapsed_ms = start.elapsed().as_millis(),
                    "Instance is ready"
                );
                return Ok(());
            }

            tracing::debug!(
                instance = %instance.config.name,
                reason = ?result.reason,
                elapsed_ms = start.elapsed().as_millis(),
                "Waiting for instance to be ready"
            );

            sleep(poll_interval).await;
        }
    }
}

#[async_trait]
impl HealthChecker for GrpcHealthChecker {
    async fn check(&self, instance: &TeiInstance) -> HealthCheckResult {
        // Check if process is running
        if !instance.is_running().await {
            return HealthCheckResult::unhealthy("Process not running".to_string());
        }

        // gRPC health check - call Info RPC to verify TEI is ready
        let addr = format!("http://localhost:{}", instance.config.port);

        // Create gRPC channel with timeout
        let channel = match tonic::transport::Channel::from_shared(addr) {
            Ok(endpoint) => {
                match endpoint
                    .timeout(Duration::from_secs(5))
                    .connect_timeout(Duration::from_secs(5))
                    .connect()
                    .await
                {
                    Ok(ch) => ch,
                    Err(e) => {
                        return HealthCheckResult::unhealthy(format!("gRPC connect failed: {}", e));
                    }
                }
            }
            Err(_) => return HealthCheckResult::unhealthy("Invalid gRPC address".to_string()),
        };

        // Call Info RPC - this only succeeds if TEI is fully ready
        use crate::grpc::proto::tei::v1::{InfoRequest, info_client::InfoClient};
        let mut client = InfoClient::new(channel);

        match client.info(InfoRequest {}).await {
            Ok(_response) => HealthCheckResult::healthy(),
            Err(e) => HealthCheckResult::unhealthy(format!("Info RPC failed: {}", e)),
        }
    }
}

/// Default restart strategy using instance.restart()
pub struct DefaultRestartStrategy;

#[async_trait]
impl RestartStrategy for DefaultRestartStrategy {
    async fn restart(&self, instance: &TeiInstance, tei_binary_path: &str) -> anyhow::Result<()> {
        instance.restart(tei_binary_path).await
    }
}

/// Metrics and logging event handler
pub struct MetricsEventHandler;

#[async_trait]
impl HealthEventHandler for MetricsEventHandler {
    async fn handle(&self, event: HealthEvent) {
        match event {
            HealthEvent::CheckStarted { .. } => {
                // No-op for now
            }
            HealthEvent::CheckSucceeded { instance_name } => {
                tracing::debug!(instance = %instance_name, "Health check succeeded");
            }
            HealthEvent::CheckFailed {
                instance_name,
                consecutive_failures,
                reason,
            } => {
                tracing::warn!(
                    instance = %instance_name,
                    failures = consecutive_failures,
                    reason = %reason,
                    "Health check failed"
                );
            }
            HealthEvent::RestartTriggered {
                instance_name,
                failure_count,
            } => {
                tracing::warn!(
                    instance = %instance_name,
                    failures = failure_count,
                    "Maximum failures reached, attempting restart"
                );
                crate::metrics::record_instance_restart(&instance_name);
            }
            HealthEvent::RestartSucceeded { instance_name } => {
                tracing::info!(instance = %instance_name, "Instance restarted successfully");
            }
            HealthEvent::RestartFailed {
                instance_name,
                error,
            } => {
                tracing::error!(
                    instance = %instance_name,
                    error = %error,
                    "Failed to restart instance"
                );
            }
            HealthEvent::StatusTransition {
                instance_name,
                from,
                to,
            } => {
                tracing::info!(
                    instance = %instance_name,
                    from = ?from,
                    to = ?to,
                    "Instance status changed"
                );
            }
        }
    }
}

// ============================================================================
// Configuration
// ============================================================================

/// Health monitor configuration
#[derive(Debug, Clone)]
pub struct HealthMonitorConfig {
    pub check_interval: Duration,
    pub initial_delay: Duration,
    pub max_failures_before_restart: u32,
    pub auto_restart: bool,
}

impl Default for HealthMonitorConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(30),
            initial_delay: Duration::from_secs(60),
            max_failures_before_restart: 3,
            auto_restart: true,
        }
    }
}

impl HealthMonitorConfig {
    pub fn builder() -> HealthMonitorConfigBuilder {
        HealthMonitorConfigBuilder::default()
    }
}

/// Builder for HealthMonitorConfig
#[derive(Default)]
pub struct HealthMonitorConfigBuilder {
    check_interval: Option<Duration>,
    initial_delay: Option<Duration>,
    max_failures_before_restart: Option<u32>,
    auto_restart: Option<bool>,
}

impl HealthMonitorConfigBuilder {
    pub fn check_interval(mut self, interval: Duration) -> Self {
        self.check_interval = Some(interval);
        self
    }

    pub fn initial_delay(mut self, delay: Duration) -> Self {
        self.initial_delay = Some(delay);
        self
    }

    pub fn max_failures_before_restart(mut self, max: u32) -> Self {
        self.max_failures_before_restart = Some(max);
        self
    }

    pub fn auto_restart(mut self, enabled: bool) -> Self {
        self.auto_restart = Some(enabled);
        self
    }

    pub fn build(self) -> HealthMonitorConfig {
        let defaults = HealthMonitorConfig::default();
        HealthMonitorConfig {
            check_interval: self.check_interval.unwrap_or(defaults.check_interval),
            initial_delay: self.initial_delay.unwrap_or(defaults.initial_delay),
            max_failures_before_restart: self
                .max_failures_before_restart
                .unwrap_or(defaults.max_failures_before_restart),
            auto_restart: self.auto_restart.unwrap_or(defaults.auto_restart),
        }
    }
}

// ============================================================================
// Health Monitor
// ============================================================================

/// Health monitor with configurable checks and auto-restart
pub struct HealthMonitor {
    registry: Arc<Registry>,
    config: HealthMonitorConfig,
    health_checker: Arc<dyn HealthChecker>,
    restart_strategy: Arc<dyn RestartStrategy>,
    event_handler: Arc<dyn HealthEventHandler>,
    tei_binary_path: Arc<str>,
}

impl HealthMonitor {
    /// Create a new health monitor with default implementations (backward compatible)
    pub fn new(
        registry: Arc<Registry>,
        check_interval_secs: u64,
        initial_delay_secs: u64,
        max_failures_before_restart: u32,
        auto_restart: bool,
        tei_binary_path: String,
    ) -> Self {
        let config = HealthMonitorConfig {
            check_interval: Duration::from_secs(check_interval_secs),
            initial_delay: Duration::from_secs(initial_delay_secs),
            max_failures_before_restart,
            auto_restart,
        };

        Self {
            registry,
            config,
            health_checker: Arc::new(GrpcHealthChecker),
            restart_strategy: Arc::new(DefaultRestartStrategy),
            event_handler: Arc::new(MetricsEventHandler),
            tei_binary_path: Arc::from(tei_binary_path),
        }
    }

    /// Create a builder for more flexible configuration
    pub fn builder(registry: Arc<Registry>) -> HealthMonitorBuilder {
        HealthMonitorBuilder::new(registry)
    }

    /// Start monitoring loop
    pub async fn run(self: Arc<Self>) {
        // Wait initial delay before first check (gives instances time to start)
        tracing::info!(
            delay_secs = self.config.initial_delay.as_secs(),
            "Waiting before starting health checks"
        );
        sleep(self.config.initial_delay).await;

        let mut ticker = interval(self.config.check_interval);

        tracing::info!(
            interval_secs = self.config.check_interval.as_secs(),
            "Health monitoring started"
        );

        loop {
            ticker.tick().await;
            self.check_all_instances().await;
        }
    }

    /// Check all instances (now public for testing)
    pub async fn check_all_instances(&self) {
        let instances = self.registry.list().await;

        for instance in instances {
            self.check_single_instance(&instance).await;
        }
    }

    /// Check a single instance (now public for testing)
    pub async fn check_single_instance(&self, instance: &TeiInstance) {
        self.event_handler
            .handle(HealthEvent::CheckStarted {
                instance_name: instance.config.name.clone(),
            })
            .await;

        let result = self.health_checker.check(instance).await;

        if result.healthy {
            self.handle_success(instance).await;
        } else {
            self.handle_failure(instance, result.reason.unwrap_or_default())
                .await;
        }
    }

    async fn handle_success(&self, instance: &TeiInstance) {
        // Reset failure count on success
        let mut stats = instance.stats.write().await;
        stats.health_check_failures = 0;
        stats.last_health_check = Some(chrono::Utc::now());

        // Update status to Running if it was Starting
        let mut status = instance.status.write().await;
        let old_status = *status;

        if old_status == InstanceStatus::Starting {
            *status = InstanceStatus::Running;

            self.event_handler
                .handle(HealthEvent::StatusTransition {
                    instance_name: instance.config.name.clone(),
                    from: old_status,
                    to: InstanceStatus::Running,
                })
                .await;
        }

        self.event_handler
            .handle(HealthEvent::CheckSucceeded {
                instance_name: instance.config.name.clone(),
            })
            .await;
    }

    async fn handle_failure(&self, instance: &TeiInstance, reason: String) {
        // Check if instance is still starting - don't count failures or restart during startup
        // This prevents premature failure marking while the instance is loading model weights
        let current_status = *instance.status.read().await;
        if current_status == InstanceStatus::Starting {
            tracing::debug!(
                instance = %instance.config.name,
                reason = %reason,
                "Health check failed for starting instance - waiting for startup to complete"
            );
            // Don't increment failure count for starting instances
            // The startup timeout (separate from health checks) handles this case
            return;
        }

        let mut stats = instance.stats.write().await;
        stats.health_check_failures += 1;
        let failures = stats.health_check_failures;

        self.event_handler
            .handle(HealthEvent::CheckFailed {
                instance_name: instance.config.name.clone(),
                consecutive_failures: failures,
                reason: reason.clone(),
            })
            .await;

        if self.config.auto_restart && failures >= self.config.max_failures_before_restart {
            self.event_handler
                .handle(HealthEvent::RestartTriggered {
                    instance_name: instance.config.name.clone(),
                    failure_count: failures,
                })
                .await;

            drop(stats); // Release lock before restart

            match self
                .restart_strategy
                .restart(instance, &self.tei_binary_path)
                .await
            {
                Ok(()) => {
                    self.event_handler
                        .handle(HealthEvent::RestartSucceeded {
                            instance_name: instance.config.name.clone(),
                        })
                        .await;
                }
                Err(e) => {
                    self.event_handler
                        .handle(HealthEvent::RestartFailed {
                            instance_name: instance.config.name.clone(),
                            error: e.to_string(),
                        })
                        .await;

                    *instance.status.write().await = InstanceStatus::Failed;
                }
            }
        }
    }
}

// ============================================================================
// Builder
// ============================================================================

pub struct HealthMonitorBuilder {
    registry: Arc<Registry>,
    config: Option<HealthMonitorConfig>,
    health_checker: Option<Arc<dyn HealthChecker>>,
    restart_strategy: Option<Arc<dyn RestartStrategy>>,
    event_handler: Option<Arc<dyn HealthEventHandler>>,
}

impl HealthMonitorBuilder {
    fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            config: None,
            health_checker: None,
            restart_strategy: None,
            event_handler: None,
        }
    }

    pub fn config(mut self, config: HealthMonitorConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn health_checker(mut self, checker: Arc<dyn HealthChecker>) -> Self {
        self.health_checker = Some(checker);
        self
    }

    pub fn restart_strategy(mut self, strategy: Arc<dyn RestartStrategy>) -> Self {
        self.restart_strategy = Some(strategy);
        self
    }

    pub fn event_handler(mut self, handler: Arc<dyn HealthEventHandler>) -> Self {
        self.event_handler = Some(handler);
        self
    }

    pub fn build(self, tei_binary_path: String) -> HealthMonitor {
        HealthMonitor {
            registry: self.registry,
            config: self.config.unwrap_or_default(),
            health_checker: self
                .health_checker
                .unwrap_or_else(|| Arc::new(GrpcHealthChecker)),
            restart_strategy: self
                .restart_strategy
                .unwrap_or_else(|| Arc::new(DefaultRestartStrategy)),
            event_handler: self
                .event_handler
                .unwrap_or_else(|| Arc::new(MetricsEventHandler)),
            tei_binary_path: Arc::from(tei_binary_path),
        }
    }
}

// ============================================================================
// Mock Implementations for Testing
// ============================================================================

#[cfg(test)]
pub mod mocks {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tokio::sync::Mutex;

    /// Mock health checker for testing
    pub struct MockHealthChecker {
        should_fail: AtomicBool,
        check_count: AtomicU32,
        failure_reason: std::sync::RwLock<String>,
    }

    impl Default for MockHealthChecker {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockHealthChecker {
        pub fn new() -> Self {
            Self {
                should_fail: AtomicBool::new(false),
                check_count: AtomicU32::new(0),
                failure_reason: std::sync::RwLock::new("Mock failure".to_string()),
            }
        }

        pub fn set_healthy(&self) {
            self.should_fail.store(false, Ordering::SeqCst);
        }

        pub fn set_unhealthy(&self, reason: String) {
            self.should_fail.store(true, Ordering::SeqCst);
            *self.failure_reason.write().unwrap() = reason;
        }

        pub fn check_count(&self) -> u32 {
            self.check_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl HealthChecker for MockHealthChecker {
        async fn check(&self, _instance: &TeiInstance) -> HealthCheckResult {
            self.check_count.fetch_add(1, Ordering::SeqCst);

            if self.should_fail.load(Ordering::SeqCst) {
                let reason = self.failure_reason.read().unwrap().clone();
                HealthCheckResult::unhealthy(reason)
            } else {
                HealthCheckResult::healthy()
            }
        }
    }

    /// Mock restart strategy for testing
    pub struct MockRestartStrategy {
        should_fail: AtomicBool,
        restart_count: AtomicU32,
        last_restarted_instance: Mutex<Option<String>>,
    }

    impl Default for MockRestartStrategy {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockRestartStrategy {
        pub fn new() -> Self {
            Self {
                should_fail: AtomicBool::new(false),
                restart_count: AtomicU32::new(0),
                last_restarted_instance: Mutex::new(None),
            }
        }

        pub fn set_should_fail(&self, should_fail: bool) {
            self.should_fail.store(should_fail, Ordering::SeqCst);
        }

        pub fn restart_count(&self) -> u32 {
            self.restart_count.load(Ordering::SeqCst)
        }

        pub async fn last_restarted_instance(&self) -> Option<String> {
            self.last_restarted_instance.lock().await.clone()
        }
    }

    #[async_trait]
    impl RestartStrategy for MockRestartStrategy {
        async fn restart(
            &self,
            instance: &TeiInstance,
            _tei_binary_path: &str,
        ) -> anyhow::Result<()> {
            self.restart_count.fetch_add(1, Ordering::SeqCst);
            *self.last_restarted_instance.lock().await = Some(instance.config.name.clone());

            if self.should_fail.load(Ordering::SeqCst) {
                anyhow::bail!("Mock restart failed");
            }

            Ok(())
        }
    }

    /// Recording event handler for testing
    pub struct RecordingEventHandler {
        events: Mutex<Vec<HealthEvent>>,
    }

    impl Default for RecordingEventHandler {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RecordingEventHandler {
        pub fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        pub async fn events(&self) -> Vec<HealthEvent> {
            self.events.lock().await.clone()
        }

        pub async fn event_count(&self) -> usize {
            self.events.lock().await.len()
        }

        pub async fn has_event_type(&self, f: impl Fn(&HealthEvent) -> bool) -> bool {
            self.events.lock().await.iter().any(f)
        }

        pub async fn clear(&self) {
            self.events.lock().await.clear();
        }
    }

    #[async_trait]
    impl HealthEventHandler for RecordingEventHandler {
        async fn handle(&self, event: HealthEvent) {
            self.events.lock().await.push(event);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InstanceConfig;

    #[test]
    fn test_health_monitor_creation() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let monitor = HealthMonitor::new(
            registry,
            30,
            60,
            3,
            true,
            "text-embeddings-router".to_string(),
        );

        assert_eq!(monitor.config.check_interval.as_secs(), 30);
        assert_eq!(monitor.config.initial_delay.as_secs(), 60);
        assert_eq!(monitor.config.max_failures_before_restart, 3);
        assert!(monitor.config.auto_restart);
    }

    #[test]
    fn test_health_monitor_builder() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let config = HealthMonitorConfig::builder()
            .check_interval(Duration::from_secs(45))
            .initial_delay(Duration::from_secs(90))
            .max_failures_before_restart(5)
            .auto_restart(false)
            .build();

        let monitor = HealthMonitor::builder(registry)
            .config(config)
            .build("tei".to_string());

        assert_eq!(monitor.config.check_interval.as_secs(), 45);
        assert_eq!(monitor.config.initial_delay.as_secs(), 90);
        assert_eq!(monitor.config.max_failures_before_restart, 5);
        assert!(!monitor.config.auto_restart);
    }

    #[tokio::test]
    async fn test_mock_health_checker() {
        use mocks::MockHealthChecker;

        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
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

        let checker = MockHealthChecker::new();

        // Test healthy
        let result = checker.check(&instance).await;
        assert!(result.healthy);
        assert_eq!(checker.check_count(), 1);

        // Test unhealthy
        checker.set_unhealthy("Connection timeout".to_string());
        let result = checker.check(&instance).await;
        assert!(!result.healthy);
        assert_eq!(result.reason, Some("Connection timeout".to_string()));
        assert_eq!(checker.check_count(), 2);

        // Test back to healthy
        checker.set_healthy();
        let result = checker.check(&instance).await;
        assert!(result.healthy);
        assert_eq!(checker.check_count(), 3);
    }

    #[tokio::test]
    async fn test_mock_restart_strategy() {
        use mocks::MockRestartStrategy;

        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let config = InstanceConfig {
            name: "test-restart".to_string(),
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

        let strategy = MockRestartStrategy::new();

        // Test successful restart
        let result = strategy.restart(&instance, "tei").await;
        assert!(result.is_ok());
        assert_eq!(strategy.restart_count(), 1);
        assert_eq!(
            strategy.last_restarted_instance().await,
            Some("test-restart".to_string())
        );

        // Test failed restart
        strategy.set_should_fail(true);
        let result = strategy.restart(&instance, "tei").await;
        assert!(result.is_err());
        assert_eq!(strategy.restart_count(), 2);
    }

    #[tokio::test]
    async fn test_recording_event_handler() {
        use mocks::RecordingEventHandler;

        let handler = RecordingEventHandler::new();

        handler
            .handle(HealthEvent::CheckStarted {
                instance_name: "test".to_string(),
            })
            .await;

        handler
            .handle(HealthEvent::CheckFailed {
                instance_name: "test".to_string(),
                consecutive_failures: 1,
                reason: "timeout".to_string(),
            })
            .await;

        assert_eq!(handler.event_count().await, 2);

        let has_failed = handler
            .has_event_type(|e| matches!(e, HealthEvent::CheckFailed { .. }))
            .await;
        assert!(has_failed);

        handler.clear().await;
        assert_eq!(handler.event_count().await, 0);
    }

    #[tokio::test]
    async fn test_health_monitor_with_mocks() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let config = InstanceConfig {
            name: "test-monitor".to_string(),
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

        let checker = Arc::new(MockHealthChecker::new());
        let restart = Arc::new(MockRestartStrategy::new());
        let events = Arc::new(RecordingEventHandler::new());

        let monitor_config = HealthMonitorConfig::builder()
            .max_failures_before_restart(3)
            .auto_restart(true)
            .build();

        let monitor = HealthMonitor::builder(registry)
            .config(monitor_config)
            .health_checker(checker.clone())
            .restart_strategy(restart.clone())
            .event_handler(events.clone())
            .build("mock".to_string());

        // Test successful check
        monitor.check_single_instance(&instance).await;
        assert_eq!(checker.check_count(), 1);
        assert!(
            events
                .has_event_type(|e| matches!(e, HealthEvent::CheckSucceeded { .. }))
                .await
        );

        // Test failure leading to restart
        checker.set_unhealthy("Connection lost".to_string());
        events.clear().await;

        for _ in 0..3 {
            monitor.check_single_instance(&instance).await;
        }

        assert_eq!(checker.check_count(), 4); // 1 success + 3 failures
        assert_eq!(restart.restart_count(), 1);
        assert!(
            events
                .has_event_type(|e| matches!(e, HealthEvent::RestartTriggered { .. }))
                .await
        );
        assert!(
            events
                .has_event_type(|e| matches!(e, HealthEvent::RestartSucceeded { .. }))
                .await
        );
    }

    #[tokio::test]
    async fn test_auto_restart_disabled() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let config = InstanceConfig {
            name: "no-restart".to_string(),
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

        let checker = Arc::new(MockHealthChecker::new());
        let restart = Arc::new(MockRestartStrategy::new());
        let events = Arc::new(RecordingEventHandler::new());

        checker.set_unhealthy("fail".to_string());

        let monitor_config = HealthMonitorConfig::builder()
            .max_failures_before_restart(3)
            .auto_restart(false) // Disabled
            .build();

        let monitor = HealthMonitor::builder(registry)
            .config(monitor_config)
            .health_checker(checker.clone())
            .restart_strategy(restart.clone())
            .event_handler(events.clone())
            .build("mock".to_string());

        // Fail many times
        for _ in 0..5 {
            monitor.check_single_instance(&instance).await;
        }

        // Should NOT have triggered restart
        assert_eq!(restart.restart_count(), 0);
        assert!(
            !events
                .has_event_type(|e| matches!(e, HealthEvent::RestartTriggered { .. }))
                .await
        );
    }

    #[tokio::test]
    async fn test_recovery_after_failure() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let config = InstanceConfig {
            name: "recovery-test".to_string(),
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

        let checker = Arc::new(MockHealthChecker::new());
        let restart = Arc::new(MockRestartStrategy::new());
        let events = Arc::new(RecordingEventHandler::new());

        let monitor_config = HealthMonitorConfig::builder()
            .max_failures_before_restart(5)
            .auto_restart(true)
            .build();

        let monitor = HealthMonitor::builder(registry)
            .config(monitor_config)
            .health_checker(checker.clone())
            .restart_strategy(restart.clone())
            .event_handler(events.clone())
            .build("mock".to_string());

        // Fail 3 times
        checker.set_unhealthy("temporary issue".to_string());
        for _ in 0..3 {
            monitor.check_single_instance(&instance).await;
        }

        // Then recover
        checker.set_healthy();
        monitor.check_single_instance(&instance).await;

        // Should NOT have triggered restart (recovered before threshold)
        assert_eq!(restart.restart_count(), 0);

        // Verify failure count was reset
        let stats = instance.stats.read().await;
        assert_eq!(stats.health_check_failures, 0);
    }

    #[tokio::test]
    async fn test_starting_instance_not_failed_by_health_checks() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let config = InstanceConfig {
            name: "starting-test".to_string(),
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

        // Set instance status to Starting (simulates a just-started instance)
        *instance.status.write().await = InstanceStatus::Starting;

        let checker = Arc::new(MockHealthChecker::new());
        let restart = Arc::new(MockRestartStrategy::new());
        let events = Arc::new(RecordingEventHandler::new());

        checker.set_unhealthy("connection refused".to_string());

        let monitor_config = HealthMonitorConfig::builder()
            .max_failures_before_restart(3)
            .auto_restart(true)
            .build();

        let monitor = HealthMonitor::builder(registry)
            .config(monitor_config)
            .health_checker(checker.clone())
            .restart_strategy(restart.clone())
            .event_handler(events.clone())
            .build("mock".to_string());

        // Fail many times while instance is Starting
        for _ in 0..10 {
            monitor.check_single_instance(&instance).await;
        }

        // Should NOT have triggered restart (instance is still Starting)
        assert_eq!(restart.restart_count(), 0);

        // Verify failure count was NOT incremented (Starting instances are skipped)
        let stats = instance.stats.read().await;
        assert_eq!(stats.health_check_failures, 0);

        // CheckFailed events should NOT have been emitted for Starting instances
        let has_failed_events = events
            .has_event_type(|e| matches!(e, HealthEvent::CheckFailed { .. }))
            .await;
        assert!(!has_failed_events);
    }

    #[tokio::test]
    async fn test_running_instance_fails_after_threshold() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let config = InstanceConfig {
            name: "running-test".to_string(),
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

        // Set instance status to Running (fully operational)
        *instance.status.write().await = InstanceStatus::Running;

        let checker = Arc::new(MockHealthChecker::new());
        let restart = Arc::new(MockRestartStrategy::new());
        let events = Arc::new(RecordingEventHandler::new());

        checker.set_unhealthy("connection refused".to_string());

        let monitor_config = HealthMonitorConfig::builder()
            .max_failures_before_restart(3)
            .auto_restart(true)
            .build();

        let monitor = HealthMonitor::builder(registry)
            .config(monitor_config)
            .health_checker(checker.clone())
            .restart_strategy(restart.clone())
            .event_handler(events.clone())
            .build("mock".to_string());

        // Fail exactly 3 times (threshold)
        for _ in 0..3 {
            monitor.check_single_instance(&instance).await;
        }

        // Should have triggered restart (Running instance exceeded threshold)
        assert_eq!(restart.restart_count(), 1);

        // CheckFailed events should have been emitted
        let has_failed_events = events
            .has_event_type(|e| matches!(e, HealthEvent::CheckFailed { .. }))
            .await;
        assert!(has_failed_events);

        // RestartTriggered should have been emitted
        let has_restart_events = events
            .has_event_type(|e| matches!(e, HealthEvent::RestartTriggered { .. }))
            .await;
        assert!(has_restart_events);
    }
}
