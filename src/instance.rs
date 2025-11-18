//! TEI instance management and process lifecycle

use crate::config::InstanceConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

/// TEI instance with process and status tracking
#[derive(Debug)]
pub struct TeiInstance {
    pub config: InstanceConfig,
    pub process: Arc<RwLock<Option<Child>>>,
    pub status: Arc<RwLock<InstanceStatus>>,
    pub stats: Arc<RwLock<InstanceStats>>,
}

/// Instance status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

/// Instance statistics
#[derive(Debug, Clone, Default, Serialize)]
pub struct InstanceStats {
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub restarts: u32,
    pub last_health_check: Option<chrono::DateTime<chrono::Utc>>,
    pub health_check_failures: u32,
}

impl TeiInstance {
    /// Create a new TEI instance (not started)
    pub fn new(config: InstanceConfig) -> Self {
        Self {
            config,
            process: Arc::new(RwLock::new(None)),
            status: Arc::new(RwLock::new(InstanceStatus::Stopped)),
            stats: Arc::new(RwLock::new(InstanceStats::default())),
        }
    }

    /// Start the TEI process
    pub async fn start(&self, tei_binary_path: &str) -> Result<()> {
        let mut cmd = Command::new(tei_binary_path);

        // Set GPU assignment if specified
        if let Some(gpu_id) = self.config.gpu_id {
            cmd.env("CUDA_VISIBLE_DEVICES", gpu_id.to_string());
            tracing::debug!(
                instance = %self.config.name,
                gpu_id = gpu_id,
                "Setting CUDA_VISIBLE_DEVICES"
            );
        }

        // Build arguments from config
        cmd.arg("--model-id").arg(&self.config.model_id);
        cmd.arg("--port").arg(self.config.port.to_string());
        cmd.arg("--max-batch-tokens")
            .arg(self.config.max_batch_tokens.to_string());
        cmd.arg("--max-concurrent-requests")
            .arg(self.config.max_concurrent_requests.to_string());
        cmd.arg("--json-output");

        if let Some(pooling) = &self.config.pooling {
            cmd.arg("--pooling").arg(pooling);
        }

        // Set Prometheus port (default auto-assigned, 0 = disabled)
        if let Some(prom_port) = self.config.prometheus_port {
            cmd.arg("--prometheus-port").arg(prom_port.to_string());
        }

        // Add extra args
        for arg in &self.config.extra_args {
            cmd.arg(arg);
        }

        // Spawn process
        let child = cmd
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn TEI process")?;

        let pid = child.id().context("Failed to get PID")?;

        *self.process.write().await = Some(child);
        *self.status.write().await = InstanceStatus::Starting;

        // Update stats
        let mut stats = self.stats.write().await;
        stats.started_at = Some(chrono::Utc::now());

        tracing::info!(
            instance = %self.config.name,
            model = %self.config.model_id,
            port = self.config.port,
            pid = pid,
            gpu_id = ?self.config.gpu_id,
            "TEI instance started"
        );

        Ok(())
    }

    /// Stop the TEI process gracefully
    pub async fn stop(&self) -> Result<()> {
        *self.status.write().await = InstanceStatus::Stopping;

        let mut process_guard = self.process.write().await;

        if let Some(mut child) = process_guard.take() {
            // Try graceful shutdown first (SIGTERM)
            if let Some(pid) = child.id() {
                #[cfg(unix)]
                {
                    use nix::sys::signal::{Signal, kill};
                    use nix::unistd::Pid;

                    let pid = Pid::from_raw(pid as i32);
                    let _ = kill(pid, Signal::SIGTERM);

                    // Wait up to 30 seconds for graceful shutdown (configurable in future)
                    tokio::select! {
                        _ = child.wait() => {
                            tracing::info!(instance = %self.config.name, "Instance stopped gracefully");
                        }
                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {
                            tracing::warn!(instance = %self.config.name, "Graceful shutdown timeout, sending SIGKILL");
                            let _ = kill(pid, Signal::SIGKILL);
                            let _ = child.wait().await;
                        }
                    }
                }

                #[cfg(not(unix))]
                {
                    // On non-Unix, just kill
                    let _ = child.kill().await;
                }
            }
        }

        *self.status.write().await = InstanceStatus::Stopped;
        Ok(())
    }

    /// Restart the instance
    pub async fn restart(&self, tei_binary_path: &str) -> Result<()> {
        tracing::info!(instance = %self.config.name, "Restarting instance");

        self.stop().await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        self.start(tei_binary_path).await?;

        let mut stats = self.stats.write().await;
        stats.restarts += 1;

        Ok(())
    }

    /// Check if process is still running
    pub async fn is_running(&self) -> bool {
        let process_guard = self.process.read().await;
        process_guard.is_some()
    }

    /// Get current PID
    pub async fn pid(&self) -> Option<u32> {
        let process_guard = self.process.read().await;
        process_guard.as_ref().and_then(|p| p.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_instance_creation() {
        let config = InstanceConfig {
            name: "test".to_string(),
            model_id: "test-model".to_string(),
            port: 9999,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            extra_args: vec![],
            created_at: None,
        };

        let instance = TeiInstance::new(config);
        assert_eq!(*instance.status.read().await, InstanceStatus::Stopped);
        assert!(!instance.is_running().await);
    }

    #[tokio::test]
    async fn test_gpu_assignment() {
        let config = InstanceConfig {
            name: "test-gpu".to_string(),
            model_id: "test-model".to_string(),
            port: 9998,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: Some(1),
            prometheus_port: None,
            extra_args: vec![],
            created_at: None,
        };

        let instance = TeiInstance::new(config);
        assert_eq!(instance.config.gpu_id, Some(1));
    }
}
