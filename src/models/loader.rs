//! Model loader for smoke testing
//!
//! Provides functionality to load a model on GPU 0, verify it works,
//! then unload it. This validates that a model is compatible with TEI.

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

/// Configuration for the model loader
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// Path to the TEI binary
    pub tei_binary_path: String,
    /// Port to use for smoke test instance
    pub smoke_test_port: u16,
    /// Timeout for model loading in seconds
    pub load_timeout_secs: u64,
}

impl Default for LoaderConfig {
    fn default() -> Self {
        Self {
            tei_binary_path: "text-embeddings-router".to_string(),
            smoke_test_port: 18080, // Use a high port unlikely to conflict
            load_timeout_secs: 300, // 5 minutes for large models
        }
    }
}

/// Model loader for smoke testing
///
/// Ensures only one smoke test runs at a time via mutex
pub struct ModelLoader {
    config: LoaderConfig,
    /// Mutex to ensure only one smoke test at a time
    lock: Mutex<()>,
}

impl ModelLoader {
    /// Create a new model loader with default configuration
    pub fn new() -> Self {
        Self {
            config: LoaderConfig::default(),
            lock: Mutex::new(()),
        }
    }

    /// Create a new model loader with custom configuration
    pub fn with_config(config: LoaderConfig) -> Self {
        Self {
            config,
            lock: Mutex::new(()),
        }
    }

    /// Create a new model loader from manager config
    pub fn from_tei_binary(tei_binary_path: String) -> Self {
        Self {
            config: LoaderConfig {
                tei_binary_path,
                ..Default::default()
            },
            lock: Mutex::new(()),
        }
    }

    /// Perform a smoke test on a model
    ///
    /// This will:
    /// 1. Start a TEI instance on GPU 0 with the specified model
    /// 2. Wait for it to become ready (model loaded successfully)
    /// 3. Shut down the instance
    ///
    /// Returns Ok(()) if the model loaded successfully, Err with details otherwise.
    pub async fn smoke_test(&self, model_id: &str) -> Result<(), String> {
        // Acquire lock to ensure only one smoke test at a time
        let _guard = self.lock.lock().await;

        tracing::info!(model_id = %model_id, "Starting smoke test");

        // Start TEI process
        let mut child = self.start_tei_process(model_id).await?;

        // Wait for ready or failure
        let result = self.wait_for_ready(&mut child, model_id).await;

        // Always try to kill the process
        tracing::info!(model_id = %model_id, "Stopping smoke test instance");
        let _ = child.kill().await;

        result
    }

    /// Start a TEI process for the given model
    async fn start_tei_process(&self, model_id: &str) -> Result<Child, String> {
        let mut cmd = Command::new(&self.config.tei_binary_path);

        cmd.arg("--model-id")
            .arg(model_id)
            .arg("--port")
            .arg(self.config.smoke_test_port.to_string())
            .arg("--max-concurrent-requests")
            .arg("1") // Minimal for smoke test
            .arg("--max-batch-tokens")
            .arg("256") // Minimal for smoke test
            .env("CUDA_VISIBLE_DEVICES", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        tracing::debug!(
            binary = %self.config.tei_binary_path,
            model_id = %model_id,
            port = %self.config.smoke_test_port,
            "Spawning TEI process for smoke test"
        );

        cmd.spawn()
            .map_err(|e| format!("Failed to spawn TEI process: {}", e))
    }

    /// Wait for TEI to become ready or fail
    async fn wait_for_ready(&self, child: &mut Child, model_id: &str) -> Result<(), String> {
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Failed to capture stderr".to_string())?;

        let mut reader = BufReader::new(stderr).lines();

        let result = timeout(
            Duration::from_secs(self.config.load_timeout_secs),
            self.monitor_output(&mut reader, model_id),
        )
        .await;

        match result {
            Ok(Ok(())) => {
                tracing::info!(model_id = %model_id, "Smoke test passed - model loaded successfully");
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::error!(model_id = %model_id, error = %e, "Smoke test failed");
                Err(e)
            }
            Err(_) => {
                tracing::error!(
                    model_id = %model_id,
                    timeout_secs = %self.config.load_timeout_secs,
                    "Smoke test timed out"
                );
                Err(format!(
                    "Timeout after {}s waiting for model to load",
                    self.config.load_timeout_secs
                ))
            }
        }
    }

    /// Monitor TEI output for ready or error indicators
    async fn monitor_output(
        &self,
        reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
        model_id: &str,
    ) -> Result<(), String> {
        while let Ok(Some(line)) = reader.next_line().await {
            tracing::trace!(model_id = %model_id, line = %line, "TEI output");

            // Check for successful startup
            // TEI logs "Started HTTP server" when ready
            if line.contains("Started HTTP server") || line.contains("Starting HTTP server") {
                return Ok(());
            }

            // Check for gRPC server started (alternative success indicator)
            if line.contains("Starting gRPC server") {
                return Ok(());
            }

            // Check for common error patterns
            if line.contains("Error") || line.contains("error:") || line.contains("CUDA error") {
                // Capture more context
                let mut error_lines = vec![line.clone()];
                // Read a few more lines for context
                for _ in 0..5 {
                    if let Ok(Some(next_line)) = reader.next_line().await {
                        error_lines.push(next_line);
                    } else {
                        break;
                    }
                }
                return Err(error_lines.join("\n"));
            }

            // Check for out of memory
            if line.contains("out of memory") || line.contains("CUDA_OUT_OF_MEMORY") {
                return Err(format!("Out of GPU memory: {}", line));
            }

            // Check for model not found
            if line.contains("404") && line.contains("not found") {
                return Err(format!("Model not found on HuggingFace: {}", model_id));
            }

            // Check for invalid model (not an embedding model)
            if line.contains("not a valid") || line.contains("Unsupported model") {
                return Err(format!("Invalid or unsupported model type: {}", line));
            }
        }

        // If we get here, process exited without success indicator
        Err("TEI process exited unexpectedly without starting".to_string())
    }
}

impl Default for ModelLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loader_config_default() {
        let config = LoaderConfig::default();
        assert_eq!(config.smoke_test_port, 18080);
        assert_eq!(config.load_timeout_secs, 300);
        assert_eq!(config.tei_binary_path, "text-embeddings-router");
    }

    #[test]
    fn test_model_loader_from_binary() {
        let loader = ModelLoader::from_tei_binary("/custom/path/tei".to_string());
        assert_eq!(loader.config.tei_binary_path, "/custom/path/tei");
        // Should inherit other defaults
        assert_eq!(loader.config.smoke_test_port, 18080);
        assert_eq!(loader.config.load_timeout_secs, 300);
    }

    #[test]
    fn test_model_loader_new() {
        let loader = ModelLoader::new();
        assert_eq!(loader.config.tei_binary_path, "text-embeddings-router");
        assert_eq!(loader.config.smoke_test_port, 18080);
    }

    #[test]
    fn test_model_loader_default() {
        let loader = ModelLoader::default();
        assert_eq!(loader.config.tei_binary_path, "text-embeddings-router");
    }

    #[test]
    fn test_model_loader_with_config() {
        let config = LoaderConfig {
            tei_binary_path: "/custom/tei".to_string(),
            smoke_test_port: 19999,
            load_timeout_secs: 600,
        };
        let loader = ModelLoader::with_config(config);
        assert_eq!(loader.config.tei_binary_path, "/custom/tei");
        assert_eq!(loader.config.smoke_test_port, 19999);
        assert_eq!(loader.config.load_timeout_secs, 600);
    }

    #[test]
    fn test_loader_config_clone() {
        let config = LoaderConfig::default();
        let cloned = config.clone();
        assert_eq!(config.tei_binary_path, cloned.tei_binary_path);
        assert_eq!(config.smoke_test_port, cloned.smoke_test_port);
    }

    #[test]
    fn test_loader_config_debug() {
        let config = LoaderConfig::default();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("LoaderConfig"));
        assert!(debug_str.contains("text-embeddings-router"));
    }

    #[tokio::test]
    async fn test_smoke_test_invalid_binary() {
        let loader = ModelLoader::from_tei_binary("/nonexistent/binary/path/tei-12345".to_string());
        let result = loader.smoke_test("BAAI/bge-small-en-v1.5").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to spawn"));
    }
}
