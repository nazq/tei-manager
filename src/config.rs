//! Configuration structures and loading logic

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/// Main manager configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ManagerConfig {
    pub api_port: u16,
    pub state_file: PathBuf,
    pub health_check_interval_secs: u64,
    pub health_check_initial_delay_secs: u64,
    pub max_failures_before_restart: u32,
    pub graceful_shutdown_timeout_secs: u64,
    pub auto_restore_on_restart: bool,
    pub max_instances: Option<usize>,
    pub instances: Vec<InstanceConfig>,

    #[serde(default = "default_tei_binary_path")]
    pub tei_binary_path: String,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            api_port: default_api_port(),
            state_file: default_state_file(),
            health_check_interval_secs: default_health_check_interval(),
            health_check_initial_delay_secs: default_health_check_initial_delay(),
            max_failures_before_restart: default_max_failures_before_restart(),
            graceful_shutdown_timeout_secs: default_graceful_shutdown_timeout(),
            auto_restore_on_restart: false,
            max_instances: None,
            instances: Vec::new(),
            tei_binary_path: default_tei_binary_path(),
        }
    }
}

impl ManagerConfig {
    /// Load configuration from file with environment variable overrides
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let mut config = if let Some(path) = path {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read config file: {:?}", path))?;
            toml::from_str(&content).context("Failed to parse TOML config")?
        } else {
            Self::default()
        };

        // Environment variable overrides
        if let Ok(port) = std::env::var("TEI_MANAGER_API_PORT") {
            config.api_port = port.parse().context("Invalid TEI_MANAGER_API_PORT value")?;
        }
        if let Ok(state_file) = std::env::var("TEI_MANAGER_STATE_FILE") {
            config.state_file = PathBuf::from(state_file);
        }
        if let Ok(interval) = std::env::var("TEI_MANAGER_HEALTH_CHECK_INTERVAL") {
            config.health_check_interval_secs = interval
                .parse()
                .context("Invalid TEI_MANAGER_HEALTH_CHECK_INTERVAL value")?;
        }
        if let Ok(binary_path) = std::env::var("TEI_BINARY_PATH") {
            config.tei_binary_path = binary_path;
        }

        Ok(config)
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        // Port range validation
        if self.api_port < 1024 {
            anyhow::bail!("API port must be >= 1024 (got {})", self.api_port);
        }

        // Check for port conflicts in seeded instances
        let mut ports = HashSet::new();
        let mut names = HashSet::new();

        for instance in &self.instances {
            // Port validation
            if instance.port < 1024 {
                anyhow::bail!(
                    "Instance '{}' port must be >= 1024 (got {})",
                    instance.name,
                    instance.port
                );
            }
            if instance.port == self.api_port {
                anyhow::bail!(
                    "Instance '{}' port {} conflicts with API port",
                    instance.name,
                    instance.port
                );
            }
            if !ports.insert(instance.port) {
                anyhow::bail!("Duplicate port {} in instance configs", instance.port);
            }

            // Name validation
            if instance.name.is_empty() {
                anyhow::bail!("Instance name cannot be empty");
            }
            if instance.name.contains('/') || instance.name.contains('\\') {
                anyhow::bail!(
                    "Instance name '{}' cannot contain path separators",
                    instance.name
                );
            }
            if !names.insert(&instance.name) {
                anyhow::bail!("Duplicate instance name: {}", instance.name);
            }
        }

        // Ensure state file directory exists or can be created
        if let Some(parent) = self.state_file.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create state file directory: {:?}", parent))?;
        }

        Ok(())
    }
}

/// Configuration for a single TEI instance
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct InstanceConfig {
    pub name: String,
    pub model_id: String,
    pub port: u16,

    #[serde(default = "default_max_batch_tokens")]
    pub max_batch_tokens: u32,

    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: u32,

    /// For SPLADE models
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pooling: Option<String>,

    /// Optional GPU assignment (sets CUDA_VISIBLE_DEVICES)
    /// If None, all GPUs are visible to this instance
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_id: Option<u32>,

    /// Prometheus metrics port for this TEI instance
    /// If None, auto-assigned starting from 9100
    /// Set to 0 to disable Prometheus metrics
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prometheus_port: Option<u16>,

    /// Additional CLI args to pass to text-embeddings-router
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// Auto-generated field (not in user config)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

// Default functions
fn default_api_port() -> u16 {
    9000
}
fn default_state_file() -> PathBuf {
    PathBuf::from("/data/tei-manager-state.toml")
}
fn default_health_check_interval() -> u64 {
    30
}
fn default_health_check_initial_delay() -> u64 {
    60
}
fn default_max_failures_before_restart() -> u32 {
    3
}
fn default_graceful_shutdown_timeout() -> u64 {
    30
}
fn default_max_batch_tokens() -> u32 {
    16384
}
fn default_max_concurrent_requests() -> u32 {
    512
}
fn default_tei_binary_path() -> String {
    "text-embeddings-router".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = ManagerConfig::default();
        assert_eq!(config.api_port, 9000);
        assert_eq!(config.health_check_interval_secs, 30);
        // Note: validate() may fail if /data doesn't exist, which is expected
        // In real usage, state_file is typically overridden to a writable location
    }

    #[test]
    fn test_load_from_file() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let config_content = r#"
api_port = 9090
health_check_interval_secs = 60
"#;
        temp_file.write_all(config_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = ManagerConfig::load(Some(temp_file.path().to_path_buf())).unwrap();
        assert_eq!(config.api_port, 9090);
        assert_eq!(config.health_check_interval_secs, 60);
    }

    #[test]
    fn test_load_from_nonexistent_file() {
        let result = ManagerConfig::load(Some(PathBuf::from("/nonexistent/config.toml")));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_invalid_toml() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"invalid toml {{").unwrap();
        temp_file.flush().unwrap();

        let result = ManagerConfig::load(Some(temp_file.path().to_path_buf()));
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_env_var_api_port_override() {
        unsafe {
            env::set_var("TEI_MANAGER_API_PORT", "9999");
        }
        let config = ManagerConfig::load(None).unwrap();
        assert_eq!(config.api_port, 9999);
        unsafe {
            env::remove_var("TEI_MANAGER_API_PORT");
        }
    }

    #[test]
    #[serial]
    fn test_env_var_api_port_invalid() {
        unsafe {
            env::set_var("TEI_MANAGER_API_PORT", "not_a_number");
        }
        let result = ManagerConfig::load(None);
        assert!(result.is_err());
        unsafe {
            env::remove_var("TEI_MANAGER_API_PORT");
        }
    }

    #[test]
    #[serial]
    fn test_env_var_state_file_override() {
        unsafe {
            env::set_var("TEI_MANAGER_STATE_FILE", "/tmp/custom-state.toml");
        }
        let config = ManagerConfig::load(None).unwrap();
        assert_eq!(config.state_file, PathBuf::from("/tmp/custom-state.toml"));
        unsafe {
            env::remove_var("TEI_MANAGER_STATE_FILE");
        }
    }

    #[test]
    #[serial]
    fn test_env_var_health_check_interval_override() {
        unsafe {
            env::set_var("TEI_MANAGER_HEALTH_CHECK_INTERVAL", "120");
        }
        let config = ManagerConfig::load(None).unwrap();
        assert_eq!(config.health_check_interval_secs, 120);
        unsafe {
            env::remove_var("TEI_MANAGER_HEALTH_CHECK_INTERVAL");
        }
    }

    #[test]
    #[serial]
    fn test_env_var_health_check_interval_invalid() {
        unsafe {
            env::set_var("TEI_MANAGER_HEALTH_CHECK_INTERVAL", "invalid");
        }
        let result = ManagerConfig::load(None);
        assert!(result.is_err());
        unsafe {
            env::remove_var("TEI_MANAGER_HEALTH_CHECK_INTERVAL");
        }
    }

    #[test]
    #[serial]
    fn test_env_var_tei_binary_path_override() {
        unsafe {
            env::set_var("TEI_BINARY_PATH", "/custom/path/to/tei");
        }
        let config = ManagerConfig::load(None).unwrap();
        assert_eq!(config.tei_binary_path, "/custom/path/to/tei");
        unsafe {
            env::remove_var("TEI_BINARY_PATH");
        }
    }

    #[test]
    fn test_state_file_directory_creation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("subdir/state.toml");

        let config = ManagerConfig {
            state_file: state_file.clone(),
            ..Default::default()
        };

        // Directory should not exist yet
        assert!(!state_file.parent().unwrap().exists());

        // validate() should create it
        config.validate().unwrap();

        // Now it should exist
        assert!(state_file.parent().unwrap().exists());
    }

    #[test]
    fn test_default_functions() {
        // Test default_max_batch_tokens
        assert_eq!(default_max_batch_tokens(), 16384);

        // Test default_max_concurrent_requests
        assert_eq!(default_max_concurrent_requests(), 512);

        // Verify they're used in InstanceConfig deserialization
        let config_json = r#"{"name":"test","model_id":"model","port":8080}"#;
        let instance: InstanceConfig = serde_json::from_str(config_json).unwrap();
        assert_eq!(instance.max_batch_tokens, 16384);
        assert_eq!(instance.max_concurrent_requests, 512);
    }

    #[test]
    fn test_port_validation() {
        let config = ManagerConfig {
            api_port: 500, // Below 1024
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_duplicate_port_detection() {
        let config = ManagerConfig {
            instances: vec![
                InstanceConfig {
                    name: "test1".to_string(),
                    model_id: "model1".to_string(),
                    port: 8080,
                    max_batch_tokens: 1024,
                    max_concurrent_requests: 10,
                    pooling: None,
                    gpu_id: None,
                    prometheus_port: None,
                    extra_args: vec![],
                    created_at: None,
                },
                InstanceConfig {
                    name: "test2".to_string(),
                    model_id: "model2".to_string(),
                    port: 8080, // Duplicate
                    max_batch_tokens: 1024,
                    max_concurrent_requests: 10,
                    pooling: None,
                    gpu_id: None,
                    prometheus_port: None,
                    extra_args: vec![],
                    created_at: None,
                },
            ],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_instance_name_validation() {
        let config = ManagerConfig {
            instances: vec![InstanceConfig {
                name: "test/invalid".to_string(), // Contains path separator
                model_id: "model1".to_string(),
                port: 8080,
                max_batch_tokens: 1024,
                max_concurrent_requests: 10,
                pooling: None,
                gpu_id: None,
                prometheus_port: None,
                extra_args: vec![],
                created_at: None,
            }],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
