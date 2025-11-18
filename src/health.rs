//! Health monitoring for TEI instances

use crate::instance::{InstanceStatus, TeiInstance};
use crate::registry::Registry;
use std::sync::Arc;
use tokio::time::{Duration, interval, sleep};

/// Health monitor with configurable checks and auto-restart
pub struct HealthMonitor {
    registry: Arc<Registry>,
    check_interval: Duration,
    initial_delay: Duration,
    auto_restart: bool,
    max_failures_before_restart: u32,
    tei_binary_path: Arc<str>,
}

impl HealthMonitor {
    /// Create a new health monitor
    pub fn new(
        registry: Arc<Registry>,
        check_interval_secs: u64,
        initial_delay_secs: u64,
        max_failures_before_restart: u32,
        auto_restart: bool,
        tei_binary_path: String,
    ) -> Self {
        Self {
            registry,
            check_interval: Duration::from_secs(check_interval_secs),
            initial_delay: Duration::from_secs(initial_delay_secs),
            auto_restart,
            max_failures_before_restart,
            tei_binary_path: Arc::from(tei_binary_path),
        }
    }

    /// Start monitoring loop
    pub async fn run(self: Arc<Self>) {
        // Wait initial delay before first check (gives instances time to start)
        tracing::info!(
            delay_secs = self.initial_delay.as_secs(),
            "Waiting before starting health checks"
        );
        sleep(self.initial_delay).await;

        let mut ticker = interval(self.check_interval);

        tracing::info!(
            interval_secs = self.check_interval.as_secs(),
            "Health monitoring started"
        );

        loop {
            ticker.tick().await;
            self.check_all_instances().await;
        }
    }

    async fn check_all_instances(&self) {
        let instances = self.registry.list().await;

        for instance in instances {
            match self.check_instance(&instance).await {
                Ok(()) => {
                    self.handle_success(&instance).await;
                }
                Err(e) => {
                    tracing::warn!(
                        instance = %instance.config.name,
                        error = %e,
                        "Health check failed"
                    );
                    self.handle_failure(&instance).await;
                }
            }
        }
    }

    async fn check_instance(&self, instance: &TeiInstance) -> anyhow::Result<()> {
        // Check if process is running
        if !instance.is_running().await {
            anyhow::bail!("Process not running");
        }

        // HTTP health check
        let url = format!("http://localhost:{}/health", instance.config.port);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;

        let response = client.get(&url).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Health check returned status: {}", response.status());
        }

        Ok(())
    }

    async fn handle_success(&self, instance: &TeiInstance) {
        // Reset failure count on success
        let mut stats = instance.stats.write().await;
        stats.health_check_failures = 0;
        stats.last_health_check = Some(chrono::Utc::now());

        // Update status to Running if it was Starting
        let mut status = instance.status.write().await;
        if *status == InstanceStatus::Starting {
            tracing::info!(
                instance = %instance.config.name,
                "Instance is now healthy"
            );
            *status = InstanceStatus::Running;
        }
    }

    async fn handle_failure(&self, instance: &TeiInstance) {
        let mut stats = instance.stats.write().await;
        stats.health_check_failures += 1;
        let failures = stats.health_check_failures;

        tracing::warn!(
            instance = %instance.config.name,
            failures = failures,
            max_failures = self.max_failures_before_restart,
            "Instance health check failed"
        );

        if self.auto_restart && failures >= self.max_failures_before_restart {
            tracing::warn!(
                instance = %instance.config.name,
                "Maximum failures reached, attempting restart"
            );

            // Record restart metric
            crate::metrics::record_instance_restart(&instance.config.name);

            drop(stats); // Release lock before restart

            if let Err(e) = instance.restart(&self.tei_binary_path).await {
                tracing::error!(
                    instance = %instance.config.name,
                    error = %e,
                    "Failed to restart instance"
                );

                *instance.status.write().await = InstanceStatus::Failed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_monitor_creation() {
        let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));
        let monitor = HealthMonitor::new(registry, 30, 60, 3, true, "text-embeddings-router".to_string());

        assert_eq!(monitor.check_interval.as_secs(), 30);
        assert_eq!(monitor.initial_delay.as_secs(), 60);
        assert_eq!(monitor.max_failures_before_restart, 3);
        assert!(monitor.auto_restart);
    }
}
