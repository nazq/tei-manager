//! Configuration structures and loading logic

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/// Main manager configuration
///
/// All fields support environment variable overrides where noted.
/// Configuration is loaded from TOML file, with env vars taking precedence.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ManagerConfig {
    /// HTTP API server port (default: 9000)
    /// Override via: TEI_MANAGER_API_PORT
    pub api_port: u16,

    /// Path to state file for persisting instance configurations (default: /data/tei-manager-state.toml)
    /// Override via: TEI_MANAGER_STATE_FILE
    pub state_file: PathBuf,

    /// Interval between health checks in seconds (default: 10)
    /// Override via: TEI_MANAGER_HEALTH_CHECK_INTERVAL
    pub health_check_interval_secs: u64,

    /// Maximum time to wait for an instance to become ready after starting (default: 300 = 5 min)
    /// If instance is still in "Starting" state after this timeout, it's considered hung.
    /// Set high enough for large models to download and load into VRAM.
    ///
    /// **Interaction with health checks**: Health check failures are NOT counted while an
    /// instance is in "Starting" status. The startup timeout is the only mechanism that
    /// can fail a starting instance. Health check-triggered restarts only apply to
    /// instances that have reached "Running" status.
    pub startup_timeout_secs: u64,

    /// Number of consecutive health check failures before restarting a running instance (default: 3)
    ///
    /// **Important**: This only applies to instances that have successfully started
    /// (status = "Running"). Instances in "Starting" status are protected from premature
    /// failure marking - use `startup_timeout_secs` to control startup failure behavior.
    pub max_failures_before_restart: u32,

    /// Graceful shutdown timeout in seconds (default: 30)
    /// Time to wait for instances to stop cleanly before force-killing
    pub graceful_shutdown_timeout_secs: u64,

    /// Auto-restore instances from state file on manager restart (default: false)
    /// When true, instances are automatically recreated from saved state
    pub auto_restore_on_restart: bool,

    /// Maximum number of instances allowed (default: None = unlimited)
    /// Set to limit resource usage on shared systems
    pub max_instances: Option<usize>,

    /// Start of port range for auto-allocation (default: 8080)
    /// When creating an instance without specifying a port, one will be
    /// auto-assigned from this range
    #[serde(default = "default_instance_port_start")]
    pub instance_port_start: u16,

    /// End of port range for auto-allocation (default: 8180)
    /// Range is [instance_port_start, instance_port_end) - 100 ports by default
    /// Must be greater than instance_port_start and have at least max_instances ports
    #[serde(default = "default_instance_port_end")]
    pub instance_port_end: u16,

    /// Seed instances to create on startup (default: empty)
    /// These are created and started automatically when the manager boots
    pub instances: Vec<InstanceConfig>,

    /// List of model IDs to pre-register in the model registry (default: empty)
    /// These models will be checked against the HF cache on startup
    /// Example: ["BAAI/bge-small-en-v1.5", "sentence-transformers/all-MiniLM-L6-v2"]
    #[serde(default)]
    pub models: Option<Vec<String>>,

    /// Path to text-embeddings-router binary (default: "text-embeddings-router")
    /// Override via: TEI_BINARY_PATH
    /// The default searches PATH; use absolute path for custom installations
    #[serde(default = "default_tei_binary_path")]
    pub tei_binary_path: String,

    /// gRPC multiplexer port (default: 9001)
    /// Override via: TEI_MANAGER_GRPC_PORT
    #[serde(default = "default_grpc_port")]
    pub grpc_port: u16,

    /// Enable gRPC multiplexer server (default: true)
    /// Override via: TEI_MANAGER_GRPC_ENABLED
    /// When disabled, only HTTP API is available
    #[serde(default = "default_grpc_enabled")]
    pub grpc_enabled: bool,

    /// gRPC max message size in MB (default: 40)
    /// Applies to both request and response messages
    /// Increase for large batch embedding requests
    #[serde(default = "default_grpc_max_message_size_mb")]
    pub grpc_max_message_size_mb: usize,

    /// Maximum parallel streaming requests per connection (default: 1024)
    /// Controls the channel buffer size for concurrent stream processing
    /// Higher values allow more parallelism but use more memory
    #[serde(default = "default_grpc_max_parallel_streams")]
    pub grpc_max_parallel_streams: usize,

    /// gRPC request timeout in seconds (default: 30)
    /// Applies to forwarded requests from multiplexer to TEI backends
    /// Set to 0 to disable timeouts (not recommended for production)
    #[serde(default = "default_grpc_request_timeout_secs")]
    pub grpc_request_timeout_secs: u64,

    /// Authentication configuration
    /// See [auth] section in config file
    #[serde(default)]
    pub auth: AuthConfig,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            api_port: default_api_port(),
            state_file: default_state_file(),
            health_check_interval_secs: default_health_check_interval(),
            startup_timeout_secs: default_startup_timeout(),
            max_failures_before_restart: default_max_failures_before_restart(),
            graceful_shutdown_timeout_secs: default_graceful_shutdown_timeout(),
            auto_restore_on_restart: false,
            max_instances: None,
            instance_port_start: default_instance_port_start(),
            instance_port_end: default_instance_port_end(),
            instances: Vec::new(),
            models: None,
            tei_binary_path: default_tei_binary_path(),
            grpc_port: default_grpc_port(),
            grpc_enabled: default_grpc_enabled(),
            grpc_max_message_size_mb: default_grpc_max_message_size_mb(),
            grpc_max_parallel_streams: default_grpc_max_parallel_streams(),
            grpc_request_timeout_secs: default_grpc_request_timeout_secs(),
            auth: AuthConfig::default(),
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
        if let Ok(port) = std::env::var("TEI_MANAGER_GRPC_PORT") {
            config.grpc_port = port
                .parse()
                .context("Invalid TEI_MANAGER_GRPC_PORT value")?;
        }
        if let Ok(enabled) = std::env::var("TEI_MANAGER_GRPC_ENABLED") {
            config.grpc_enabled = enabled
                .parse()
                .context("Invalid TEI_MANAGER_GRPC_ENABLED value")?;
        }

        Ok(config)
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        // Port range validation
        if self.api_port < 1024 {
            anyhow::bail!("API port must be >= 1024 (got {})", self.api_port);
        }

        // Instance port range validation
        if self.instance_port_start < 1024 {
            anyhow::bail!(
                "instance_port_start must be >= 1024 (got {})",
                self.instance_port_start
            );
        }
        if self.instance_port_end <= self.instance_port_start {
            anyhow::bail!(
                "instance_port_end ({}) must be greater than instance_port_start ({})",
                self.instance_port_end,
                self.instance_port_start
            );
        }

        // Check port range can fit max_instances
        let port_range_size = (self.instance_port_end - self.instance_port_start) as usize;
        if let Some(max) = self.max_instances
            && port_range_size < max
        {
            anyhow::bail!(
                "Port range [{}, {}) only has {} ports but max_instances is {}",
                self.instance_port_start,
                self.instance_port_end,
                port_range_size,
                max
            );
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
            if self.grpc_enabled && instance.port == self.grpc_port {
                anyhow::bail!(
                    "Instance '{}' port {} conflicts with gRPC port",
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

        // Validate auth configuration
        if self.auth.enabled {
            if self.auth.providers.is_empty() {
                anyhow::bail!("Authentication is enabled but no providers are configured");
            }

            // Validate mTLS config if mtls provider is enabled
            if self.auth.providers.contains(&"mtls".to_string()) {
                let mtls = self.auth.mtls.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("mTLS provider enabled but mtls config missing")
                })?;

                // Check certificate files exist
                if !mtls.ca_cert.exists() {
                    anyhow::bail!("mTLS CA certificate not found: {:?}", mtls.ca_cert);
                }
                if !mtls.server_cert.exists() {
                    anyhow::bail!("mTLS server certificate not found: {:?}", mtls.server_cert);
                }
                if !mtls.server_key.exists() {
                    anyhow::bail!("mTLS server key not found: {:?}", mtls.server_key);
                }

                // Warn about insecure settings
                if mtls.allow_self_signed {
                    eprintln!(
                        "WARNING: mTLS allow_self_signed=true - this should only be used in development"
                    );
                }
            }
        }

        Ok(())
    }
}

/// Configuration for a single TEI instance
///
/// Used both in config file [[instances]] sections and via HTTP API
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct InstanceConfig {
    /// Unique name for this instance (required)
    /// Used as identifier in API calls and state management
    /// Cannot contain path separators (/ or \)
    #[serde(default)]
    pub name: String,

    /// HuggingFace model ID (required)
    /// Examples: "BAAI/bge-small-en-v1.5", "sentence-transformers/all-mpnet-base-v2"
    #[serde(default)]
    pub model_id: String,

    /// Port for this instance's HTTP server (default: auto-assigned from port range)
    /// Must be >= 1024 and unique across all instances
    #[serde(default)]
    pub port: u16,

    /// Maximum batch tokens for embedding requests (default: 16384)
    /// Controls memory usage and throughput
    #[serde(default = "default_max_batch_tokens")]
    pub max_batch_tokens: u32,

    /// Maximum concurrent requests per instance (default: 512)
    /// Higher values increase throughput but use more memory
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: u32,

    /// Pooling strategy for sequence output (default: None)
    /// Used for SPLADE models: "splade" or for custom pooling
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pooling: Option<String>,

    /// Optional GPU assignment (default: None = all GPUs visible)
    /// Sets CUDA_VISIBLE_DEVICES for this instance
    /// Pin to specific GPU: gpu_id = 0 or gpu_id = 1
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_id: Option<u32>,

    /// Prometheus metrics port for this TEI instance (default: auto-assigned from 9100)
    /// Set to 0 to disable Prometheus metrics for this instance
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prometheus_port: Option<u16>,

    /// Override startup timeout for this instance in seconds (default: uses global setting)
    /// Use for large models that need more time to download and load into VRAM
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,

    /// Additional CLI args to pass to text-embeddings-router (default: empty)
    /// Example: ["--dtype", "float16", "--revision", "main"]
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// Auto-generated timestamp when instance was created (internal use)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Authentication configuration
///
/// Configure authentication providers for both HTTP API and gRPC servers.
/// Currently supports mTLS (mutual TLS) authentication.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct AuthConfig {
    /// Enable authentication (default: false)
    /// When disabled, all endpoints are publicly accessible
    pub enabled: bool,

    /// List of enabled auth providers (default: empty)
    /// Supported: ["mtls"]
    pub providers: Vec<String>,

    /// Require certificate headers from reverse proxy (default: false)
    ///
    /// When true, requests without X-SSL-Client-Cert headers will be rejected.
    /// Use this in production when running behind nginx/envoy to ensure
    /// attackers cannot bypass authentication by connecting directly to the
    /// API port.
    ///
    /// When false (default), requests without cert headers are assumed to be
    /// native TLS connections where rustls already verified the client cert.
    /// This is ONLY safe if your API port is not directly accessible from
    /// untrusted networks.
    ///
    /// SECURITY WARNING: If require_cert_headers=false and your API is directly
    /// accessible (not behind a reverse proxy), authentication can be bypassed.
    #[serde(default)]
    pub require_cert_headers: bool,

    /// mTLS configuration (required if "mtls" is in providers)
    pub mtls: Option<MtlsConfig>,
}

/// mTLS (mutual TLS) authentication configuration
///
/// Requires client certificates signed by a trusted CA.
/// Both HTTP and gRPC servers use the same TLS configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MtlsConfig {
    /// Path to CA certificate for verifying client certs (required)
    /// All client certificates must be signed by this CA
    pub ca_cert: PathBuf,

    /// Path to server certificate (required)
    /// Must be signed by a CA trusted by clients
    pub server_cert: PathBuf,

    /// Path to server private key (required)
    /// Must match server_cert
    pub server_key: PathBuf,

    /// Allow self-signed certificates (default: false)
    /// WARNING: Only for development - disables CA chain verification
    #[serde(default)]
    pub allow_self_signed: bool,

    /// Verify client certificate subject (CN/O/OU) (default: true)
    /// When true, checks against allowed_subjects list
    #[serde(default = "default_verify_subject")]
    pub verify_subject: bool,

    /// Allowed certificate subjects (default: empty = allow all)
    /// List of allowed CN values, e.g., ["CN=client1", "CN=client2"]
    #[serde(default)]
    pub allowed_subjects: Vec<String>,

    /// Verify Subject Alternative Names (SAN) (default: false)
    /// When true, checks against allowed_sans list
    #[serde(default)]
    pub verify_san: bool,

    /// Allowed SANs (default: empty = allow all)
    /// List of allowed SAN values, e.g., ["DNS:client.example.com"]
    #[serde(default)]
    pub allowed_sans: Vec<String>,
}

// Default functions
fn default_api_port() -> u16 {
    9000
}
fn default_state_file() -> PathBuf {
    PathBuf::from("/data/tei-manager-state.toml")
}
fn default_health_check_interval() -> u64 {
    10
}
fn default_startup_timeout() -> u64 {
    300 // 5 minutes - enough for large model downloads
}
fn default_max_failures_before_restart() -> u32 {
    3
}
fn default_instance_port_start() -> u16 {
    8080
}
fn default_instance_port_end() -> u16 {
    8180 // 100 ports by default
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
fn default_grpc_port() -> u16 {
    9001
}
fn default_grpc_enabled() -> bool {
    true
}
fn default_grpc_max_message_size_mb() -> usize {
    40
}
fn default_grpc_max_parallel_streams() -> usize {
    1024
}
fn default_grpc_request_timeout_secs() -> u64 {
    30
}
fn default_verify_subject() -> bool {
    true
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests intentionally use env::set_var to test env parsing
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
        assert_eq!(config.health_check_interval_secs, 10);
        assert_eq!(config.startup_timeout_secs, 300);
        // Note: validate() may fail if /data doesn't exist, which is expected
        // In real usage, state_file is typically overridden to a writable location
    }

    #[test]
    #[serial]
    fn test_load_from_file() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let config_content = r"
api_port = 9090
health_check_interval_secs = 60
";
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
                    ..Default::default()
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
                    ..Default::default()
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
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
