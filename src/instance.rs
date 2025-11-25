//! TEI instance management and process lifecycle

use crate::config::InstanceConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

// ============================================================================
// Trait Definitions
// ============================================================================

/// Configuration for spawning a TEI process
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub instance_name: String,
    pub binary_path: String,
    pub model_id: String,
    pub port: u16,
    pub max_batch_tokens: u32,
    pub max_concurrent_requests: u32,
    pub pooling: Option<String>,
    pub gpu_id: Option<u32>,
    pub prometheus_port: Option<u16>,
    pub extra_args: Vec<String>,
}

/// Opaque handle to a spawned process
#[derive(Debug, Clone)]
pub struct ProcessHandle {
    pub(crate) id: String,
}

/// Trait for managing process lifecycle
#[async_trait]
pub trait ProcessManager: Send + Sync {
    /// Spawn a new TEI process
    async fn spawn(&self, config: SpawnConfig) -> Result<ProcessHandle>;

    /// Stop a process gracefully with timeout
    async fn stop(&self, handle: ProcessHandle, timeout: Duration) -> Result<()>;

    /// Check if process is running
    async fn is_running(&self, handle: &ProcessHandle) -> bool;

    /// Get process ID
    async fn pid(&self, handle: &ProcessHandle) -> Option<u32>;
}

// ============================================================================
// Production Implementation
// ============================================================================

/// Production process manager using tokio::process
pub struct SystemProcessManager {
    processes: Arc<RwLock<std::collections::HashMap<String, Child>>>,
}

impl SystemProcessManager {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }
}

impl Default for SystemProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProcessManager for SystemProcessManager {
    async fn spawn(&self, config: SpawnConfig) -> Result<ProcessHandle> {
        let mut cmd = Command::new(&config.binary_path);

        // Set GPU assignment if specified
        if let Some(gpu_id) = config.gpu_id {
            cmd.env("CUDA_VISIBLE_DEVICES", gpu_id.to_string());
            tracing::debug!(gpu_id = gpu_id, "Setting CUDA_VISIBLE_DEVICES");
        }

        // Build arguments from config
        cmd.arg("--model-id").arg(&config.model_id);
        cmd.arg("--port").arg(config.port.to_string());
        cmd.arg("--max-batch-tokens")
            .arg(config.max_batch_tokens.to_string());
        cmd.arg("--max-concurrent-requests")
            .arg(config.max_concurrent_requests.to_string());
        cmd.arg("--json-output");

        if let Some(pooling) = &config.pooling {
            cmd.arg("--pooling").arg(pooling);
        }

        // Set Prometheus port if provided
        let has_prometheus_port_in_extra_args = config
            .extra_args
            .iter()
            .any(|arg| arg == "--prometheus-port");

        if !has_prometheus_port_in_extra_args && let Some(prom_port) = config.prometheus_port {
            cmd.arg("--prometheus-port").arg(prom_port.to_string());
        }

        // Add extra args
        for arg in &config.extra_args {
            cmd.arg(arg);
        }

        // Setup log file redirection
        // Use env var if set, otherwise try /data/logs, fallback to /tmp/tei-manager/logs
        let log_dir_path =
            std::env::var("TEI_MANAGER_LOG_DIR").unwrap_or_else(|_| "/data/logs".to_string());

        let log_dir = std::path::Path::new(&log_dir_path);

        // Try to create the directory, fall back to /tmp if it fails
        let log_dir = if let Err(e) = std::fs::create_dir_all(log_dir) {
            tracing::warn!(
                error = %e,
                attempted_dir = %log_dir_path,
                "Failed to create log directory, falling back to /tmp/tei-manager/logs"
            );
            let fallback = std::path::Path::new("/tmp/tei-manager/logs");
            std::fs::create_dir_all(fallback).context("Failed to create fallback log directory")?;
            fallback
        } else {
            log_dir
        };

        let log_path = log_dir.join(format!("{}.log", config.instance_name));
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("Failed to open log file: {:?}", log_path))?;

        let stdout_file = log_file
            .try_clone()
            .context("Failed to clone log file for stdout")?;
        let stderr_file = log_file
            .try_clone()
            .context("Failed to clone log file for stderr")?;

        // Spawn process
        let child = cmd
            .stdout(stdout_file)
            .stderr(stderr_file)
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn TEI process")?;

        let pid = child.id().context("Failed to get PID")?;
        let handle_id = format!("process_{}", pid);

        tracing::info!(
            model = %config.model_id,
            port = config.port,
            pid = pid,
            gpu_id = ?config.gpu_id,
            "TEI process spawned"
        );

        let handle = ProcessHandle {
            id: handle_id.clone(),
        };

        self.processes.write().await.insert(handle_id, child);

        Ok(handle)
    }

    async fn stop(&self, handle: ProcessHandle, timeout: Duration) -> Result<()> {
        let mut processes = self.processes.write().await;

        if let Some(mut child) = processes.remove(&handle.id) {
            // Try graceful shutdown first (SIGTERM)
            if let Some(pid) = child.id() {
                #[cfg(unix)]
                {
                    use nix::sys::signal::{Signal, kill};
                    use nix::unistd::Pid;

                    let pid = Pid::from_raw(pid as i32);
                    let _ = kill(pid, Signal::SIGTERM);

                    // Wait for graceful shutdown with timeout
                    tokio::select! {
                        _ = child.wait() => {
                            tracing::info!("Process stopped gracefully");
                        }
                        _ = tokio::time::sleep(timeout) => {
                            tracing::warn!("Graceful shutdown timeout, sending SIGKILL");
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

        Ok(())
    }

    async fn is_running(&self, handle: &ProcessHandle) -> bool {
        let processes = self.processes.read().await;
        processes.contains_key(&handle.id)
    }

    async fn pid(&self, handle: &ProcessHandle) -> Option<u32> {
        let processes = self.processes.read().await;
        processes.get(&handle.id).and_then(|p| p.id())
    }
}

// ============================================================================
// TEI Instance with Dependency Injection
// ============================================================================

/// TEI instance with process and status tracking
pub struct TeiInstance {
    pub config: InstanceConfig,
    process_manager: Arc<dyn ProcessManager>,
    process_handle: Arc<RwLock<Option<ProcessHandle>>>,
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
    /// Create a new TEI instance with custom process manager
    pub fn new_with_manager(config: InstanceConfig, manager: Arc<dyn ProcessManager>) -> Self {
        Self {
            config,
            process_manager: manager,
            process_handle: Arc::new(RwLock::new(None)),
            status: Arc::new(RwLock::new(InstanceStatus::Stopped)),
            stats: Arc::new(RwLock::new(InstanceStats::default())),
        }
    }

    /// Create a new TEI instance with default system process manager
    pub fn new(config: InstanceConfig) -> Self {
        Self::new_with_manager(config, Arc::new(SystemProcessManager::new()))
    }

    /// Start the TEI process
    pub async fn start(&self, tei_binary_path: &str) -> Result<()> {
        let spawn_config = SpawnConfig {
            instance_name: self.config.name.clone(),
            binary_path: tei_binary_path.to_string(),
            model_id: self.config.model_id.clone(),
            port: self.config.port,
            max_batch_tokens: self.config.max_batch_tokens,
            max_concurrent_requests: self.config.max_concurrent_requests,
            pooling: self.config.pooling.clone(),
            gpu_id: self.config.gpu_id,
            prometheus_port: self.config.prometheus_port,
            extra_args: self.config.extra_args.clone(),
        };

        let handle = self.process_manager.spawn(spawn_config).await?;
        let pid = self.process_manager.pid(&handle).await;

        *self.process_handle.write().await = Some(handle);
        *self.status.write().await = InstanceStatus::Starting;

        // Update stats
        let mut stats = self.stats.write().await;
        stats.started_at = Some(chrono::Utc::now());

        tracing::info!(
            instance = %self.config.name,
            model = %self.config.model_id,
            port = self.config.port,
            pid = ?pid,
            gpu_id = ?self.config.gpu_id,
            "TEI instance started"
        );

        Ok(())
    }

    /// Stop the TEI process gracefully
    pub async fn stop(&self) -> Result<()> {
        *self.status.write().await = InstanceStatus::Stopping;

        let mut handle_guard = self.process_handle.write().await;

        if let Some(handle) = handle_guard.take() {
            self.process_manager
                .stop(handle, Duration::from_secs(30))
                .await?;

            tracing::info!(instance = %self.config.name, "Instance stopped");
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
        let handle_guard = self.process_handle.read().await;
        if let Some(handle) = handle_guard.as_ref() {
            self.process_manager.is_running(handle).await
        } else {
            false
        }
    }

    /// Get current PID
    pub async fn pid(&self) -> Option<u32> {
        let handle_guard = self.process_handle.read().await;
        if let Some(handle) = handle_guard.as_ref() {
            self.process_manager.pid(handle).await
        } else {
            None
        }
    }
}

// ============================================================================
// Mock Implementation for Testing
// ============================================================================

#[cfg(test)]
pub mod mocks {
    use super::*;
    use std::collections::HashMap;

    /// Mock process manager for testing
    pub struct MockProcessManager {
        processes: Arc<RwLock<HashMap<String, ProcessState>>>,
        next_id: Arc<RwLock<u32>>,
    }

    #[derive(Debug, Clone)]
    struct ProcessState {
        pid: u32,
        running: bool,
        config: SpawnConfig,
    }

    impl Default for MockProcessManager {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockProcessManager {
        pub fn new() -> Self {
            Self {
                processes: Arc::new(RwLock::new(HashMap::new())),
                next_id: Arc::new(RwLock::new(1000)),
            }
        }

        /// Get the number of active processes
        pub async fn process_count(&self) -> usize {
            self.processes.read().await.len()
        }

        /// Check if a process was spawned with specific config
        pub async fn was_spawned_with(&self, model_id: &str, port: u16) -> bool {
            let processes = self.processes.read().await;
            processes
                .values()
                .any(|p| p.config.model_id == model_id && p.config.port == port)
        }

        /// Get spawn config for a handle
        pub async fn get_config(&self, handle: &ProcessHandle) -> Option<SpawnConfig> {
            let processes = self.processes.read().await;
            processes.get(&handle.id).map(|p| p.config.clone())
        }
    }

    #[async_trait]
    impl ProcessManager for MockProcessManager {
        async fn spawn(&self, config: SpawnConfig) -> Result<ProcessHandle> {
            let mut next_id = self.next_id.write().await;
            let pid = *next_id;
            *next_id += 1;

            let handle_id = format!("mock_process_{}", pid);
            let handle = ProcessHandle {
                id: handle_id.clone(),
            };

            let state = ProcessState {
                pid,
                running: true,
                config,
            };

            self.processes.write().await.insert(handle_id, state);

            Ok(handle)
        }

        async fn stop(&self, handle: ProcessHandle, _timeout: Duration) -> Result<()> {
            let mut processes = self.processes.write().await;
            processes.remove(&handle.id);
            Ok(())
        }

        async fn is_running(&self, handle: &ProcessHandle) -> bool {
            let processes = self.processes.read().await;
            processes
                .get(&handle.id)
                .map(|p| p.running)
                .unwrap_or(false)
        }

        async fn pid(&self, handle: &ProcessHandle) -> Option<u32> {
            let processes = self.processes.read().await;
            processes.get(&handle.id).map(|p| p.pid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mocks::MockProcessManager;

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
            ..Default::default()
        };

        let manager = Arc::new(MockProcessManager::new());
        let instance = TeiInstance::new_with_manager(config, manager);
        assert_eq!(*instance.status.read().await, InstanceStatus::Stopped);
        assert!(!instance.is_running().await);
    }

    #[tokio::test]
    async fn test_instance_start() {
        let config = InstanceConfig {
            name: "test-start".to_string(),
            model_id: "bert-base".to_string(),
            port: 8080,
            max_batch_tokens: 2048,
            max_concurrent_requests: 20,
            pooling: Some("mean".to_string()),
            gpu_id: Some(0),
            prometheus_port: Some(9090),
            extra_args: vec!["--trust-remote-code".to_string()],
            ..Default::default()
        };

        let manager = Arc::new(MockProcessManager::new());
        let instance = TeiInstance::new_with_manager(config, manager.clone());

        instance.start("/usr/bin/tei").await.unwrap();

        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);
        assert!(instance.is_running().await);
        assert!(instance.pid().await.is_some());

        // Verify spawn config
        assert!(manager.was_spawned_with("bert-base", 8080).await);
    }

    #[tokio::test]
    async fn test_instance_stop() {
        let config = InstanceConfig {
            name: "test-stop".to_string(),
            model_id: "test-model".to_string(),
            port: 8081,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let manager = Arc::new(MockProcessManager::new());
        let instance = TeiInstance::new_with_manager(config, manager.clone());

        instance.start("/usr/bin/tei").await.unwrap();
        assert_eq!(manager.process_count().await, 1);

        instance.stop().await.unwrap();
        assert_eq!(*instance.status.read().await, InstanceStatus::Stopped);
        assert!(!instance.is_running().await);
        assert_eq!(manager.process_count().await, 0);
    }

    #[tokio::test]
    async fn test_instance_restart() {
        let config = InstanceConfig {
            name: "test-restart".to_string(),
            model_id: "test-model".to_string(),
            port: 8082,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let manager = Arc::new(MockProcessManager::new());
        let instance = TeiInstance::new_with_manager(config, manager.clone());

        instance.start("/usr/bin/tei").await.unwrap();
        let initial_pid = instance.pid().await.unwrap();

        instance.restart("/usr/bin/tei").await.unwrap();
        let new_pid = instance.pid().await.unwrap();

        assert_ne!(initial_pid, new_pid);
        assert_eq!(instance.stats.read().await.restarts, 1);
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
            ..Default::default()
        };

        let manager = Arc::new(MockProcessManager::new());
        let instance = TeiInstance::new_with_manager(config, manager.clone());
        instance.start("/usr/bin/tei").await.unwrap();

        assert_eq!(instance.config.gpu_id, Some(1));
    }

    #[tokio::test]
    async fn test_process_handle_lifecycle() {
        let config = InstanceConfig {
            name: "test-handle".to_string(),
            model_id: "test-model".to_string(),
            port: 8083,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let manager = Arc::new(MockProcessManager::new());
        let instance = TeiInstance::new_with_manager(config, manager);

        // Initially no handle
        assert!(instance.process_handle.read().await.is_none());

        // After start, handle exists
        instance.start("/usr/bin/tei").await.unwrap();
        assert!(instance.process_handle.read().await.is_some());

        // After stop, handle is removed
        instance.stop().await.unwrap();
        assert!(instance.process_handle.read().await.is_none());
    }

    #[tokio::test]
    async fn test_stats_tracking() {
        let config = InstanceConfig {
            name: "test-stats".to_string(),
            model_id: "test-model".to_string(),
            port: 8084,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let manager = Arc::new(MockProcessManager::new());
        let instance = TeiInstance::new_with_manager(config, manager);

        // Initially no started_at
        assert!(instance.stats.read().await.started_at.is_none());

        instance.start("/usr/bin/tei").await.unwrap();

        // After start, started_at is set
        assert!(instance.stats.read().await.started_at.is_some());

        // Restart increments counter
        instance.restart("/usr/bin/tei").await.unwrap();
        assert_eq!(instance.stats.read().await.restarts, 1);

        instance.restart("/usr/bin/tei").await.unwrap();
        assert_eq!(instance.stats.read().await.restarts, 2);
    }

    #[tokio::test]
    async fn test_spawn_config_propagation() {
        let config = InstanceConfig {
            name: "test-config".to_string(),
            model_id: "custom-model".to_string(),
            port: 7777,
            max_batch_tokens: 4096,
            max_concurrent_requests: 50,
            pooling: Some("cls".to_string()),
            gpu_id: Some(2),
            prometheus_port: Some(9999),
            extra_args: vec!["--arg1".to_string(), "--arg2".to_string()],
            ..Default::default()
        };

        let manager = Arc::new(MockProcessManager::new());
        let instance = TeiInstance::new_with_manager(config.clone(), manager.clone());

        instance.start("/custom/path/tei").await.unwrap();

        // Verify config was propagated correctly
        let handle = instance.process_handle.read().await;
        let spawn_config = manager.get_config(handle.as_ref().unwrap()).await.unwrap();

        assert_eq!(spawn_config.binary_path, "/custom/path/tei");
        assert_eq!(spawn_config.model_id, "custom-model");
        assert_eq!(spawn_config.port, 7777);
        assert_eq!(spawn_config.max_batch_tokens, 4096);
        assert_eq!(spawn_config.max_concurrent_requests, 50);
        assert_eq!(spawn_config.pooling, Some("cls".to_string()));
        assert_eq!(spawn_config.gpu_id, Some(2));
        assert_eq!(spawn_config.prometheus_port, Some(9999));
        assert_eq!(spawn_config.extra_args.len(), 2);
    }

    #[tokio::test]
    async fn test_multiple_instances() {
        let manager = Arc::new(MockProcessManager::new());

        let config1 = InstanceConfig {
            name: "inst1".to_string(),
            model_id: "model1".to_string(),
            port: 8001,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let config2 = InstanceConfig {
            name: "inst2".to_string(),
            model_id: "model2".to_string(),
            port: 8002,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };

        let inst1 = TeiInstance::new_with_manager(config1, manager.clone());
        let inst2 = TeiInstance::new_with_manager(config2, manager.clone());

        inst1.start("/usr/bin/tei").await.unwrap();
        inst2.start("/usr/bin/tei").await.unwrap();

        assert_eq!(manager.process_count().await, 2);

        inst1.stop().await.unwrap();
        assert_eq!(manager.process_count().await, 1);

        inst2.stop().await.unwrap();
        assert_eq!(manager.process_count().await, 0);
    }
}
