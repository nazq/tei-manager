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

            // A process that has already exited will never become ready; fail fast
            // instead of polling until the timeout.
            if !instance.is_running().await {
                let reason = exit_reason(instance, result.reason).await;
                anyhow::bail!(
                    "Instance '{}' exited during startup: {}",
                    instance.config.name,
                    reason
                );
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

/// Best description of why a process is gone: its recorded exit status if the
/// child has been reaped, otherwise whatever the last health probe reported.
async fn exit_reason(instance: &TeiInstance, probe_reason: Option<String>) -> String {
    match instance.exit_status().await {
        Some(exit) => exit.to_string(),
        None => probe_reason.unwrap_or_else(|| "process not running".to_string()),
    }
}

#[async_trait]
impl HealthChecker for GrpcHealthChecker {
    async fn check(&self, instance: &TeiInstance) -> HealthCheckResult {
        // Check if process is running (this reaps an exited child)
        if !instance.is_running().await {
            let reason = match instance.exit_status().await {
                Some(exit) => exit.to_string(),
                None => "process not running".to_string(),
            };
            return HealthCheckResult::unhealthy(reason);
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
    /// How long an instance may stay in `Starting` before it is marked `Failed`.
    /// A per-instance `startup_timeout_secs` overrides this.
    pub startup_timeout: Duration,
    pub max_failures_before_restart: u32,
    pub auto_restart: bool,
    /// At the startup timeout, a live process whose log was written within
    /// this window is treated as still loading and given more time; only a
    /// dead process or a stalled log fails the instance.
    pub startup_log_stall: Duration,
}

impl Default for HealthMonitorConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(30),
            initial_delay: Duration::from_secs(60),
            startup_timeout: Duration::from_secs(crate::config::DEFAULT_STARTUP_TIMEOUT_SECS),
            max_failures_before_restart: 3,
            auto_restart: true,
            startup_log_stall: Duration::from_secs(crate::config::DEFAULT_STARTUP_LOG_STALL_SECS),
        }
    }
}

impl HealthMonitor {
    /// Override the startup log-stall window (builder-style, used by main)
    pub fn with_startup_log_stall(mut self, window: Duration) -> Self {
        self.config.startup_log_stall = window;
        self
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
    startup_timeout: Option<Duration>,
    max_failures_before_restart: Option<u32>,
    auto_restart: Option<bool>,
    startup_log_stall: Option<Duration>,
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

    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = Some(timeout);
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

    pub fn startup_log_stall(mut self, window: Duration) -> Self {
        self.startup_log_stall = Some(window);
        self
    }

    pub fn build(self) -> HealthMonitorConfig {
        let defaults = HealthMonitorConfig::default();
        HealthMonitorConfig {
            check_interval: self.check_interval.unwrap_or(defaults.check_interval),
            initial_delay: self.initial_delay.unwrap_or(defaults.initial_delay),
            startup_timeout: self.startup_timeout.unwrap_or(defaults.startup_timeout),
            max_failures_before_restart: self
                .max_failures_before_restart
                .unwrap_or(defaults.max_failures_before_restart),
            auto_restart: self.auto_restart.unwrap_or(defaults.auto_restart),
            startup_log_stall: self.startup_log_stall.unwrap_or(defaults.startup_log_stall),
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
    ///
    /// The first check runs after one `check_interval`; instances that are still
    /// loading are protected by `startup_timeout_secs`, not by delaying the monitor.
    pub fn new(
        registry: Arc<Registry>,
        check_interval_secs: u64,
        startup_timeout_secs: u64,
        max_failures_before_restart: u32,
        auto_restart: bool,
        tei_binary_path: String,
    ) -> Self {
        let config = HealthMonitorConfig {
            check_interval: Duration::from_secs(check_interval_secs),
            initial_delay: Duration::from_secs(check_interval_secs),
            startup_timeout: Duration::from_secs(startup_timeout_secs),
            max_failures_before_restart,
            auto_restart,
            ..Default::default()
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

    /// Decide whether a Starting instance has definitively failed to start.
    async fn startup_failure(&self, instance: &TeiInstance, reason: &str) -> Option<String> {
        if !instance.is_running().await {
            let reason = exit_reason(instance, Some(reason.to_string())).await;
            return Some(format!("exited during startup: {reason}"));
        }

        let timeout = instance.config.startup_timeout(self.config.startup_timeout);
        let started_at = instance.stats.read().await.started_at?;
        let elapsed = (chrono::Utc::now() - started_at)
            .to_std()
            .unwrap_or_default();
        if elapsed <= timeout {
            return None;
        }

        // Past the timeout — but a live process still writing its log is
        // loading (weight download/conversion), not hung. Give it more time
        // rather than restarting it and losing the progress.
        if let Some(written) = instance.last_log_write().await
            && let Ok(idle) = std::time::SystemTime::now().duration_since(written)
            && idle < self.config.startup_log_stall
        {
            tracing::warn!(
                instance = %instance.config.name,
                elapsed_secs = elapsed.as_secs(),
                timeout_secs = timeout.as_secs(),
                log_idle_secs = idle.as_secs(),
                "Startup timeout exceeded but process is alive and its log is active; still loading"
            );
            return None;
        }

        Some(format!(
            "did not become ready within {}s (last check: {reason})",
            timeout.as_secs()
        ))
    }

    async fn handle_failure(&self, instance: &TeiInstance, reason: String) {
        // While an instance is Starting, failed checks are expected (model still loading)
        // and are not counted. Two things end the grace period: the process exiting, or
        // the startup timeout elapsing. Either marks the instance Failed with a reason.
        let current_status = *instance.status.read().await;
        if current_status == InstanceStatus::Starting {
            if let Some(failure) = self.startup_failure(instance, &reason).await {
                instance.mark_failed(failure).await;
                self.event_handler
                    .handle(HealthEvent::StatusTransition {
                        instance_name: instance.config.name.clone(),
                        from: InstanceStatus::Starting,
                        to: InstanceStatus::Failed,
                    })
                    .await;
            } else {
                tracing::debug!(
                    instance = %instance.config.name,
                    reason = %reason,
                    "Health check failed for starting instance - waiting for startup to complete"
                );
            }
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

                    instance.mark_failed(format!("restart failed: {e}")).await;
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
        assert_eq!(monitor.config.initial_delay.as_secs(), 30);
        assert_eq!(monitor.config.startup_timeout.as_secs(), 60);
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
            .startup_timeout(Duration::from_secs(120))
            .max_failures_before_restart(5)
            .auto_restart(false)
            .build();

        let monitor = HealthMonitor::builder(registry)
            .config(config)
            .build("tei".to_string());

        assert_eq!(monitor.config.check_interval.as_secs(), 45);
        assert_eq!(monitor.config.initial_delay.as_secs(), 90);
        assert_eq!(monitor.config.startup_timeout.as_secs(), 120);
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
        use crate::instance::mocks::MockProcessManager;
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

        // A live (mock) process that has not become ready yet
        let instance = Arc::new(TeiInstance::new_with_manager(
            config,
            Arc::new(MockProcessManager::new()),
        ));
        instance.start("mock").await.unwrap();
        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);

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

        // Still starting: within the timeout and the process is alive
        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);
        assert!(instance.stats.read().await.last_error.is_none());
    }

    /// Shared scaffolding for the startup-failure tests below
    async fn starting_instance_with_mock(
        name: &str,
        startup_timeout_secs: Option<u64>,
    ) -> (
        Arc<TeiInstance>,
        Arc<crate::instance::mocks::MockProcessManager>,
    ) {
        use crate::instance::mocks::MockProcessManager;

        let manager = Arc::new(MockProcessManager::new());
        let config = InstanceConfig {
            name: name.to_string(),
            model_id: "model".to_string(),
            port: 8080,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            startup_timeout_secs,
            ..Default::default()
        };
        let instance = Arc::new(TeiInstance::new_with_manager(config, manager.clone()));
        instance.start("mock").await.unwrap();
        (instance, manager)
    }

    fn monitor_with(
        checker: Arc<dyn HealthChecker>,
        restart: Arc<dyn RestartStrategy>,
        events: Arc<dyn HealthEventHandler>,
        config: HealthMonitorConfig,
    ) -> HealthMonitor {
        let registry = Arc::new(Registry::new(None, "mock".to_string(), 8080, 8180));
        HealthMonitor::builder(registry)
            .config(config)
            .health_checker(checker)
            .restart_strategy(restart)
            .event_handler(events)
            .build("mock".to_string())
    }

    #[tokio::test]
    async fn test_starting_instance_with_active_log_survives_timeout() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        // 1s startup timeout, already elapsed — but the log is fresh
        let (instance, manager) = starting_instance_with_mock("loading", Some(1)).await;
        instance.stats.write().await.started_at =
            Some(chrono::Utc::now() - chrono::Duration::seconds(60));
        manager
            .set_last_log_write(Some(std::time::SystemTime::now()))
            .await;

        let checker = Arc::new(MockHealthChecker::new());
        checker.set_unhealthy("not ready".to_string());
        let monitor = monitor_with(
            checker,
            Arc::new(MockRestartStrategy::new()),
            Arc::new(RecordingEventHandler::new()),
            HealthMonitorConfig::default(),
        );
        monitor.check_single_instance(&instance).await;
        assert_eq!(
            *instance.status.read().await,
            InstanceStatus::Starting,
            "live process with active log must keep loading"
        );

        // Log goes stale → now it fails
        manager
            .set_last_log_write(Some(
                std::time::SystemTime::now() - Duration::from_secs(3600),
            ))
            .await;
        monitor.check_single_instance(&instance).await;
        assert_eq!(*instance.status.read().await, InstanceStatus::Failed);
        let stats = instance.stats.read().await;
        assert!(
            stats
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("did not become ready"),
            "{:?}",
            stats.last_error
        );
    }

    #[tokio::test]
    async fn test_starting_instance_stall_window_configurable() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let (instance, manager) = starting_instance_with_mock("stall-cfg", Some(1)).await;
        instance.stats.write().await.started_at =
            Some(chrono::Utc::now() - chrono::Duration::seconds(60));
        // Log written 10s ago: inside the default 60s window, outside a 5s one
        manager
            .set_last_log_write(Some(std::time::SystemTime::now() - Duration::from_secs(10)))
            .await;

        let checker = Arc::new(MockHealthChecker::new());
        checker.set_unhealthy("not ready".to_string());
        let monitor = monitor_with(
            checker,
            Arc::new(MockRestartStrategy::new()),
            Arc::new(RecordingEventHandler::new()),
            HealthMonitorConfig::builder()
                .startup_log_stall(Duration::from_secs(5))
                .build(),
        );
        monitor.check_single_instance(&instance).await;
        assert_eq!(*instance.status.read().await, InstanceStatus::Failed);
    }

    #[tokio::test]
    async fn test_starting_instance_fails_when_process_exits() {
        use crate::instance::ProcessExit;
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let (instance, manager) = starting_instance_with_mock("exits", None).await;

        let checker = Arc::new(MockHealthChecker::new());
        let restart = Arc::new(MockRestartStrategy::new());
        let events = Arc::new(RecordingEventHandler::new());
        let monitor = monitor_with(
            checker.clone(),
            restart.clone(),
            events.clone(),
            HealthMonitorConfig::default(),
        );

        // Alive but not ready: stays Starting
        checker.set_unhealthy("gRPC connect failed".to_string());
        monitor.check_single_instance(&instance).await;
        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);

        // Process dies (e.g. compute-cap mismatch): must flip to Failed immediately
        manager
            .exit_all(ProcessExit {
                code: Some(1),
                signal: None,
                last_log_error: Some("Runtime compute cap 120 is not compatible".to_string()),
            })
            .await;
        checker.set_unhealthy("process exited with code 1".to_string());
        monitor.check_single_instance(&instance).await;

        assert_eq!(*instance.status.read().await, InstanceStatus::Failed);
        let err = instance.stats.read().await.last_error.clone().unwrap();
        assert!(err.contains("exited during startup"), "{err}");
        assert!(err.contains("process exited with code 1"), "{err}");

        // Exactly one Starting -> Failed transition, no restart during startup
        assert_eq!(restart.restart_count(), 0);
        let transitions = events
            .has_event_type(|e| {
                matches!(
                    e,
                    HealthEvent::StatusTransition {
                        from: InstanceStatus::Starting,
                        to: InstanceStatus::Failed,
                        ..
                    }
                )
            })
            .await;
        assert!(transitions);
    }

    #[tokio::test]
    async fn test_starting_instance_fails_after_startup_timeout() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let (instance, _manager) = starting_instance_with_mock("slow", None).await;

        let checker = Arc::new(MockHealthChecker::new());
        checker.set_unhealthy("gRPC connect failed".to_string());
        let monitor = monitor_with(
            checker,
            Arc::new(MockRestartStrategy::new()),
            Arc::new(RecordingEventHandler::new()),
            HealthMonitorConfig::builder()
                .startup_timeout(Duration::from_secs(300))
                .build(),
        );

        // Within the timeout: still Starting
        monitor.check_single_instance(&instance).await;
        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);

        // Pretend it started long ago
        instance.stats.write().await.started_at =
            Some(chrono::Utc::now() - chrono::Duration::seconds(301));
        monitor.check_single_instance(&instance).await;

        assert_eq!(*instance.status.read().await, InstanceStatus::Failed);
        let err = instance.stats.read().await.last_error.clone().unwrap();
        assert!(err.contains("did not become ready within 300s"), "{err}");
    }

    #[tokio::test]
    async fn test_per_instance_startup_timeout_overrides_global() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        // Instance allows 1000s even though the monitor default is 300s
        let (instance, _manager) = starting_instance_with_mock("big-model", Some(1000)).await;
        let checker = Arc::new(MockHealthChecker::new());
        checker.set_unhealthy("gRPC connect failed".to_string());
        let monitor = monitor_with(
            checker,
            Arc::new(MockRestartStrategy::new()),
            Arc::new(RecordingEventHandler::new()),
            HealthMonitorConfig::builder()
                .startup_timeout(Duration::from_secs(300))
                .build(),
        );

        instance.stats.write().await.started_at =
            Some(chrono::Utc::now() - chrono::Duration::seconds(600));
        monitor.check_single_instance(&instance).await;
        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);

        instance.stats.write().await.started_at =
            Some(chrono::Utc::now() - chrono::Duration::seconds(1001));
        monitor.check_single_instance(&instance).await;
        assert_eq!(*instance.status.read().await, InstanceStatus::Failed);
    }

    #[tokio::test]
    async fn test_wait_for_ready_bails_when_process_exits() {
        use crate::instance::ProcessExit;

        let (instance, manager) = starting_instance_with_mock("dead", None).await;
        manager
            .exit_all(ProcessExit {
                code: Some(2),
                signal: None,
                last_log_error: None,
            })
            .await;

        // Would wait 60s if it only honoured the timeout; must return promptly
        let start = std::time::Instant::now();
        let err = GrpcHealthChecker::wait_for_ready(
            &instance,
            Duration::from_secs(60),
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();

        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(err.to_string().contains("exited during startup"), "{err}");
        assert!(
            err.to_string().contains("process exited with code 2"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn test_failed_restart_records_reason() {
        use mocks::{MockHealthChecker, MockRestartStrategy, RecordingEventHandler};

        let (instance, _manager) = starting_instance_with_mock("restart-fails", None).await;
        *instance.status.write().await = InstanceStatus::Running;

        let checker = Arc::new(MockHealthChecker::new());
        checker.set_unhealthy("Info RPC failed".to_string());
        let restart = Arc::new(MockRestartStrategy::new());
        restart.set_should_fail(true);
        let monitor = monitor_with(
            checker,
            restart.clone(),
            Arc::new(RecordingEventHandler::new()),
            HealthMonitorConfig::builder()
                .max_failures_before_restart(2)
                .auto_restart(true)
                .build(),
        );

        monitor.check_single_instance(&instance).await;
        assert_eq!(*instance.status.read().await, InstanceStatus::Running);
        monitor.check_single_instance(&instance).await;

        assert_eq!(restart.restart_count(), 1);
        assert_eq!(*instance.status.read().await, InstanceStatus::Failed);
        let err = instance.stats.read().await.last_error.clone().unwrap();
        assert!(err.contains("restart failed: Mock restart failed"), "{err}");
    }

    #[tokio::test]
    async fn test_wait_for_ready_times_out_while_process_alive() {
        // Mock process stays alive but nothing listens on the port, so the
        // gRPC probe keeps failing until the timeout.
        let (instance, _manager) = starting_instance_with_mock("never-ready", None).await;

        let err = GrpcHealthChecker::wait_for_ready(
            &instance,
            Duration::from_millis(300),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("did not become ready within"),
            "{err}"
        );
        assert!(instance.is_running().await);
        // wait_for_ready reports; the caller decides how to mark the instance
        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);
    }

    #[tokio::test]
    async fn test_restart_puts_instance_back_into_starting_with_fresh_error() {
        use crate::instance::ProcessExit;

        let (instance, manager) = starting_instance_with_mock("cycle", None).await;
        manager
            .exit_all(ProcessExit {
                code: Some(1),
                signal: None,
                last_log_error: None,
            })
            .await;
        instance.mark_failed("boom").await;
        assert!(instance.stats.read().await.last_error.is_some());

        instance.restart("mock").await.unwrap();
        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);
        assert!(instance.stats.read().await.last_error.is_none());
        assert!(instance.is_running().await);
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
