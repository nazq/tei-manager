//! TEI instance management and process lifecycle

use crate::config::InstanceConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
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
    /// `RUST_LOG` filter for the child process
    pub log_level: String,
}

/// Opaque handle to a spawned process
#[derive(Debug, Clone)]
pub struct ProcessHandle {
    pub(crate) id: String,
}

/// How a process terminated, plus the most useful line from its log
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    /// Exit code, if the process exited normally
    pub code: Option<i32>,
    /// Terminating signal number, if the process was killed by a signal
    pub signal: Option<i32>,
    /// Last error-level line from the process log, if one could be found
    pub last_log_error: Option<String>,
}

impl std::fmt::Display for ProcessExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "process exited with code {code}")?,
            (None, Some(sig)) => write!(f, "process killed by signal {sig}")?,
            (None, None) => write!(f, "process exited")?,
        }
        if let Some(line) = &self.last_log_error {
            write!(f, ": {line}")?;
        }
        Ok(())
    }
}

/// Trait for managing process lifecycle
#[async_trait]
pub trait ProcessManager: Send + Sync {
    /// Spawn a new TEI process
    async fn spawn(&self, config: SpawnConfig) -> Result<ProcessHandle>;

    /// Stop a process gracefully with timeout
    async fn stop(&self, handle: ProcessHandle, timeout: Duration) -> Result<()>;

    /// Check if process is running.
    ///
    /// Implementations must reap exited children here so a dead process is
    /// never reported as alive.
    async fn is_running(&self, handle: &ProcessHandle) -> bool;

    /// Get process ID
    async fn pid(&self, handle: &ProcessHandle) -> Option<u32>;

    /// How the process terminated, if it has been observed to exit
    async fn exit_status(&self, handle: &ProcessHandle) -> Option<ProcessExit>;

    /// When the process last wrote to its log, if known.
    ///
    /// Used to distinguish a live process that is still making progress
    /// (e.g. converting model weights) from one that has hung.
    async fn last_log_write(&self, _handle: &ProcessHandle) -> Option<std::time::SystemTime> {
        None
    }

    /// Evidence from the process log that TEI fell back to CPU, if any.
    ///
    /// Used to catch an instance that is healthy but silently serving
    /// embeddings on CPU (e.g. the host driver is older than the image's
    /// CUDA userspace).
    async fn gpu_fallback_evidence(&self, _handle: &ProcessHandle) -> Option<String> {
        None
    }
}

/// Read the last error-level line from a TEI log file.
///
/// TEI writes JSON lines with `"level":"ERROR"`; anyhow's final report is a plain
/// `Error: ...` line. Only the tail of the file is scanned.
pub(crate) fn last_log_error(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    const TAIL_BYTES: u64 = 16 * 1024;

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;

    // Prefer TEI's structured ERROR line (it carries the specific cause) over
    // anyhow's generic `Error: ...` trailer.
    let json_error = buf
        .lines()
        .rev()
        .filter(|l| l.contains("\"level\":\"ERROR\""))
        .find_map(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()?
                .get("message")?
                .as_str()
                .map(str::to_string)
        });
    json_error.or_else(|| {
        buf.lines()
            .rev()
            .find(|l| l.starts_with("Error:"))
            .map(|l| l.trim().to_string())
    })
}

/// Scan the head of a TEI log for evidence that the model fell back to CPU.
///
/// When the CUDA userspace cannot initialize (e.g. the host driver supports
/// an older CUDA than the image requires), TEI logs `Could not create
/// backend` warnings followed by `Using CPU instead` — early in the log,
/// right after startup — and then serves embeddings on CPU. Returns the
/// message of the last matching line. A later `... on Cuda(...)` line clears
/// earlier evidence: instance logs are opened in append mode across
/// restarts, and only the latest run's backend choice counts.
pub(crate) fn gpu_fallback_evidence(path: &std::path::Path) -> Option<String> {
    use std::io::Read;

    const HEAD_BYTES: u64 = 64 * 1024;

    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(HEAD_BYTES).read_to_end(&mut bytes).ok()?;
    let buf = String::from_utf8_lossy(&bytes);

    let mut evidence = None;
    for line in buf.lines() {
        if line.contains("on Cuda(") {
            evidence = None;
        } else if line.contains("Using CPU instead") || line.contains("Could not create backend") {
            let message = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| Some(v.get("message")?.as_str()?.to_string()))
                .unwrap_or_else(|| line.trim().to_string());
            evidence = Some(message);
        }
    }
    evidence
}

// ============================================================================
// Production Implementation
// ============================================================================

/// Production process manager using tokio::process
pub struct SystemProcessManager {
    processes: Arc<RwLock<HashMap<String, Child>>>,
    /// Log file per handle, used to surface the failure reason after exit
    log_paths: Arc<RwLock<HashMap<String, PathBuf>>>,
    /// Exit status of children that have been reaped
    exited: Arc<RwLock<HashMap<String, ProcessExit>>>,
}

impl SystemProcessManager {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(RwLock::new(HashMap::new())),
            log_paths: Arc::new(RwLock::new(HashMap::new())),
            exited: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Reap `child` if it has exited, recording how it terminated.
    /// Returns true if the child is still running.
    async fn reap_if_exited(&self, handle_id: &str, child: &mut Child) -> bool {
        let status = match child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => return true,
            Err(e) => {
                tracing::warn!(handle = handle_id, error = %e, "try_wait failed");
                return true;
            }
        };

        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;

        let last_log_error = self
            .log_paths
            .read()
            .await
            .get(handle_id)
            .and_then(|p| last_log_error(p));

        let exit = ProcessExit {
            code: status.code(),
            signal,
            last_log_error,
        };
        tracing::error!(handle = handle_id, exit = %exit, "TEI process exited");
        self.exited
            .write()
            .await
            .insert(handle_id.to_string(), exit);
        false
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

        // TEI logs every embedded input at info level; keep the child quiet
        // unless the instance asks for more.
        cmd.env("RUST_LOG", &config.log_level);

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

        self.log_paths
            .write()
            .await
            .insert(handle_id.clone(), log_path);
        self.exited.write().await.remove(&handle_id);
        self.processes.write().await.insert(handle_id, child);

        Ok(handle)
    }

    async fn stop(&self, handle: ProcessHandle, timeout: Duration) -> Result<()> {
        let mut processes = self.processes.write().await;
        self.log_paths.write().await.remove(&handle.id);
        self.exited.write().await.remove(&handle.id);

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
        let mut processes = self.processes.write().await;
        let Some(child) = processes.get_mut(&handle.id) else {
            return false;
        };
        if self.reap_if_exited(&handle.id, child).await {
            return true;
        }
        processes.remove(&handle.id);
        false
    }

    async fn pid(&self, handle: &ProcessHandle) -> Option<u32> {
        let processes = self.processes.read().await;
        processes.get(&handle.id).and_then(|p| p.id())
    }

    async fn exit_status(&self, handle: &ProcessHandle) -> Option<ProcessExit> {
        self.exited.read().await.get(&handle.id).cloned()
    }

    async fn last_log_write(&self, handle: &ProcessHandle) -> Option<std::time::SystemTime> {
        let path = self.log_paths.read().await.get(&handle.id).cloned()?;
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    async fn gpu_fallback_evidence(&self, handle: &ProcessHandle) -> Option<String> {
        let path = self.log_paths.read().await.get(&handle.id).cloned()?;
        gpu_fallback_evidence(&path)
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
    /// Lifetime restart count (manual and automatic)
    pub restarts: u32,
    pub last_health_check: Option<chrono::DateTime<chrono::Utc>>,
    pub health_check_failures: u32,
    /// Why the instance last transitioned to `Failed`, if it has
    pub last_error: Option<String>,
    /// When the health monitor last attempted an automatic restart.
    /// In-memory backoff bookkeeping only; never persisted to the state file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_restart_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Consecutive automatic (health-monitor) restart attempts since the
    /// instance last passed a health check or was manually restarted. Drives
    /// restart backoff and the `max_restarts` give-up; `restarts` above keeps
    /// its lifetime-total meaning.
    pub backoff_restarts: u32,
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
            log_level: self.config.log_level().to_string(),
        };

        let handle = self.process_manager.spawn(spawn_config).await?;
        let pid = self.process_manager.pid(&handle).await;

        *self.process_handle.write().await = Some(handle);
        *self.status.write().await = InstanceStatus::Starting;

        // Update stats
        let mut stats = self.stats.write().await;
        stats.started_at = Some(chrono::Utc::now());
        stats.last_error = None;

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

    /// How the process terminated, if it has exited
    /// When this instance's TEI process last wrote to its log, if known
    pub async fn last_log_write(&self) -> Option<std::time::SystemTime> {
        let handle = self.process_handle.read().await.clone()?;
        self.process_manager.last_log_write(&handle).await
    }

    /// Evidence from this instance's TEI log that it fell back to CPU, if any
    pub async fn gpu_fallback_evidence(&self) -> Option<String> {
        let handle = self.process_handle.read().await.clone()?;
        self.process_manager.gpu_fallback_evidence(&handle).await
    }

    pub async fn exit_status(&self) -> Option<ProcessExit> {
        let handle_guard = self.process_handle.read().await;
        match handle_guard.as_ref() {
            Some(handle) => self.process_manager.exit_status(handle).await,
            None => None,
        }
    }

    /// Clear automatic-restart backoff bookkeeping.
    ///
    /// Called when an operator manually starts or restarts the instance via
    /// the API: manual intervention means the health monitor should manage the
    /// instance from a clean slate again (fresh backoff, fresh restart budget).
    pub async fn reset_restart_backoff(&self) {
        let mut stats = self.stats.write().await;
        stats.backoff_restarts = 0;
        stats.last_restart_at = None;
    }

    /// Transition to `Failed`, recording the reason for the API and logs
    pub async fn mark_failed(&self, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::error!(instance = %self.config.name, reason = %reason, "Instance failed");
        self.stats.write().await.last_error = Some(reason);
        *self.status.write().await = InstanceStatus::Failed;
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
        log_write: Arc<RwLock<Option<std::time::SystemTime>>>,
        fallback_evidence: Arc<RwLock<Option<String>>>,
    }

    #[derive(Debug, Clone)]
    struct ProcessState {
        pid: u32,
        running: bool,
        config: SpawnConfig,
        exit: Option<ProcessExit>,
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
                log_write: Arc::new(RwLock::new(None)),
                fallback_evidence: Arc::new(RwLock::new(None)),
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

        /// Set what `last_log_write` reports for every process
        pub async fn set_last_log_write(&self, when: Option<std::time::SystemTime>) {
            *self.log_write.write().await = when;
        }

        /// Set what `gpu_fallback_evidence` reports for every process
        pub async fn set_gpu_fallback_evidence(&self, line: Option<String>) {
            *self.fallback_evidence.write().await = line;
        }

        /// Simulate every spawned process having exited with `exit`
        pub async fn exit_all(&self, exit: ProcessExit) {
            for state in self.processes.write().await.values_mut() {
                state.running = false;
                state.exit = Some(exit.clone());
            }
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
                exit: None,
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

        async fn last_log_write(&self, _handle: &ProcessHandle) -> Option<std::time::SystemTime> {
            *self.log_write.read().await
        }

        async fn gpu_fallback_evidence(&self, _handle: &ProcessHandle) -> Option<String> {
            self.fallback_evidence.read().await.clone()
        }

        async fn exit_status(&self, handle: &ProcessHandle) -> Option<ProcessExit> {
            let processes = self.processes.read().await;
            processes.get(&handle.id).and_then(|p| p.exit.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mocks::MockProcessManager;

    fn write_log(dir: &tempfile::TempDir, lines: &[&str]) -> PathBuf {
        let path = dir.path().join("x.log");
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn test_last_log_error_extracts_json_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(
            &dir,
            &[
                r#"{"level":"INFO","message":"Starting model backend"}"#,
                r#"{"level":"ERROR","message":"Could not start Candle backend: compute cap 120"}"#,
                "Error: Could not create backend",
                "",
                "Caused by:",
                "    Could not start backend",
            ],
        );
        // The structured ERROR line carries the real cause and wins over the
        // trailing plain `Error:` report.
        assert_eq!(
            last_log_error(&path).as_deref(),
            Some("Could not start Candle backend: compute cap 120")
        );
    }

    #[test]
    fn test_last_log_error_falls_back_to_plain_error_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(
            &dir,
            &[
                r#"{"level":"INFO","message":"hello"}"#,
                "Error: Could not create backend",
                "Caused by: something",
            ],
        );
        assert_eq!(
            last_log_error(&path).as_deref(),
            Some("Error: Could not create backend")
        );
    }

    #[test]
    fn test_last_log_error_parses_json_error_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(
            &dir,
            &[
                r#"{"level":"INFO","message":"hello"}"#,
                r#"{"level":"ERROR","message":"boom","target":"x"}"#,
                r#"{"level":"INFO","message":"after"}"#,
            ],
        );
        assert_eq!(last_log_error(&path).as_deref(), Some("boom"));
    }

    #[test]
    fn test_last_log_error_none_when_no_errors_or_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(&dir, &[r#"{"level":"INFO","message":"fine"}"#]);
        assert_eq!(last_log_error(&path), None);
        assert_eq!(last_log_error(&dir.path().join("missing.log")), None);
    }

    #[test]
    fn test_gpu_fallback_evidence_detects_cpu_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(
            &dir,
            &[
                r#"{"level":"INFO","message":"Args { model_id: \"BAAI/bge-small-en-v1.5\" }"}"#,
                r#"{"level":"WARN","message":"Could not create backend","target":"text_embeddings_backend"}"#,
                r#"{"level":"WARN","message":"Could not create backend","target":"text_embeddings_backend"}"#,
                r#"{"level":"WARN","message":"Using CPU instead","target":"text_embeddings_backend"}"#,
                r#"{"level":"INFO","message":"Ready"}"#,
            ],
        );
        // The last matching line wins and its JSON message is extracted
        assert_eq!(
            gpu_fallback_evidence(&path).as_deref(),
            Some("Using CPU instead")
        );
    }

    #[test]
    fn test_gpu_fallback_evidence_backend_failure_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(
            &dir,
            &["Could not create backend: CUDA_ERROR_COMPAT_NOT_SUPPORTED_ON_DEVICE"],
        );
        // Non-JSON line: returned verbatim
        assert_eq!(
            gpu_fallback_evidence(&path).as_deref(),
            Some("Could not create backend: CUDA_ERROR_COMPAT_NOT_SUPPORTED_ON_DEVICE")
        );
    }

    #[test]
    fn test_gpu_fallback_evidence_none_for_healthy_cuda_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(
            &dir,
            &[
                r#"{"level":"INFO","message":"Starting FlashBert model on Cuda(CudaDevice(DeviceId(1)))"}"#,
                r#"{"level":"INFO","message":"Ready"}"#,
            ],
        );
        assert_eq!(gpu_fallback_evidence(&path), None);
        assert_eq!(gpu_fallback_evidence(&dir.path().join("missing.log")), None);
    }

    #[test]
    fn test_gpu_fallback_evidence_cleared_by_later_cuda_start() {
        // Logs are appended across restarts: a failed CPU run followed by a
        // successful GPU run must not be flagged.
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(
            &dir,
            &[
                r#"{"level":"WARN","message":"Could not create backend"}"#,
                r#"{"level":"WARN","message":"Using CPU instead"}"#,
                r#"{"level":"INFO","message":"Starting FlashBert model on Cuda(CudaDevice(DeviceId(0)))"}"#,
            ],
        );
        assert_eq!(gpu_fallback_evidence(&path), None);
    }

    #[test]
    fn test_process_exit_display() {
        let code = ProcessExit {
            code: Some(1),
            signal: None,
            last_log_error: Some("bad cap".to_string()),
        };
        assert_eq!(code.to_string(), "process exited with code 1: bad cap");

        let sig = ProcessExit {
            code: None,
            signal: Some(9),
            last_log_error: None,
        };
        assert_eq!(sig.to_string(), "process killed by signal 9");

        let unknown = ProcessExit {
            code: None,
            signal: None,
            last_log_error: None,
        };
        assert_eq!(unknown.to_string(), "process exited");
    }

    /// Real child process: a binary that exits immediately must be reaped and
    /// reported as not running, with its exit code captured.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_system_manager_sets_child_rust_log() {
        // A stand-in "router" that records its RUST_LOG next to itself
        let dir =
            std::env::temp_dir().join(format!("tei-manager-rust-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-router.sh");
        let out = dir.join("out.txt");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"rust_log=$RUST_LOG\" > '{}'\n",
                out.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manager = SystemProcessManager::new();
        let handle = manager
            .spawn(SpawnConfig {
                instance_name: "rust-log-test".to_string(),
                binary_path: script.to_string_lossy().to_string(),
                model_id: "m".to_string(),
                port: 1,
                max_batch_tokens: 1,
                max_concurrent_requests: 1,
                pooling: None,
                gpu_id: None,
                prometheus_port: None,
                extra_args: vec![],
                log_level: "text_embeddings_router=debug".to_string(),
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while manager.is_running(&handle).await && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let recorded = std::fs::read_to_string(&out).unwrap();
        assert_eq!(recorded.trim(), "rust_log=text_embeddings_router=debug");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_system_manager_reaps_exited_child() {
        let manager = SystemProcessManager::new();
        let handle = manager
            .spawn(SpawnConfig {
                instance_name: "reap-test".to_string(),
                binary_path: "/bin/false".to_string(),
                model_id: "m".to_string(),
                port: 1,
                max_batch_tokens: 1,
                max_concurrent_requests: 1,
                pooling: None,
                gpu_id: None,
                prometheus_port: None,
                extra_args: vec![],
                log_level: "warn".to_string(),
            })
            .await
            .unwrap();

        // Poll until the child has exited and been reaped
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while manager.is_running(&handle).await {
            assert!(std::time::Instant::now() < deadline, "child never exited");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(!manager.is_running(&handle).await);
        assert_eq!(manager.pid(&handle).await, None);
        let exit = manager.exit_status(&handle).await.expect("exit recorded");
        assert_eq!(exit.code, Some(1));
        assert_eq!(exit.signal, None);

        // stop() on an already-reaped handle is a no-op and clears the record
        manager
            .stop(handle.clone(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(manager.exit_status(&handle).await, None);
    }

    #[tokio::test]
    async fn test_mark_failed_records_reason_and_start_clears_it() {
        let instance = TeiInstance::new_with_manager(
            InstanceConfig {
                name: "mf".to_string(),
                model_id: "m".to_string(),
                port: 9999,
                ..Default::default()
            },
            Arc::new(MockProcessManager::new()),
        );

        instance.mark_failed("compute cap mismatch").await;
        assert_eq!(*instance.status.read().await, InstanceStatus::Failed);
        assert_eq!(
            instance.stats.read().await.last_error.as_deref(),
            Some("compute cap mismatch")
        );

        instance.start("mock").await.unwrap();
        assert_eq!(*instance.status.read().await, InstanceStatus::Starting);
        assert!(instance.stats.read().await.last_error.is_none());
        assert_eq!(instance.exit_status().await, None);
    }

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
