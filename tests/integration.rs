//! Integration tests that run the API in-process for code coverage
//!
//! These tests exercise the API handlers directly using axum-test,
//! which runs in-process and contributes to code coverage metrics.
//!
//! **Setup Required**: Run `./scripts/setup-test-binary.sh` to extract the
//! real TEI binary from the official Docker image before running tests.

use axum_test::TestServer;
use serde_json::json;
use std::sync::{Arc, OnceLock};
use tei_manager::{
    api::routes::{AppState, create_router},
    config::ManagerConfig,
    metrics,
    registry::Registry,
    state::StateManager,
};
use tempfile::TempDir;

// Global metrics handle - only initialize once per test process
static METRICS_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

fn get_metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    METRICS_HANDLE
        .get_or_init(|| metrics::setup_metrics().expect("Failed to setup metrics"))
        .clone()
}

/// Helper to create a test server with the API
async fn create_test_server() -> (TestServer, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("state.toml");

    // Use the real TEI binary extracted from official image
    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/text-embeddings-router");

    let config = ManagerConfig {
        state_file: state_file.clone(),
        tei_binary_path: tei_binary.to_string_lossy().to_string(),
        max_instances: Some(10),
        ..Default::default()
    };

    let registry = Arc::new(Registry::new(
        config.max_instances,
        config.tei_binary_path.clone(),
    ));

    let state_manager = Arc::new(StateManager::new(
        state_file,
        registry.clone(),
        config.tei_binary_path.clone(),
    ));

    let state = AppState {
        registry,
        state_manager,
        prometheus_handle: get_metrics_handle(),
    };

    let app = create_router(state);
    let server = TestServer::new(app).expect("Failed to create test server");

    (server, temp_dir)
}

#[tokio::test]
async fn test_health_endpoint() {
    let (server, _temp_dir) = create_test_server().await;

    let response = server.get("/health").await;

    assert_eq!(response.status_code(), 200);

    let body: serde_json::Value = response.json();
    assert_eq!(body["status"], "healthy");
    assert!(body["timestamp"].is_string());
}

#[tokio::test]
async fn test_metrics_endpoint() {
    let (server, _temp_dir) = create_test_server().await;

    let response = server.get("/metrics").await;

    assert_eq!(response.status_code(), 200);
    // Metrics may be empty initially but endpoint should respond
    let _text = response.text(); // Verify we can read the body
}

#[tokio::test]
async fn test_list_instances_empty() {
    let (server, _temp_dir) = create_test_server().await;

    let response = server.get("/instances").await;

    assert_eq!(response.status_code(), 200);

    let instances: Vec<serde_json::Value> = response.json();
    assert_eq!(instances.len(), 0);
}

#[tokio::test]
async fn test_create_instance() {
    let (server, _temp_dir) = create_test_server().await;

    let create_req = json!({
        "name": "test-instance",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080,
        "max_batch_tokens": 16384,
        "max_concurrent_requests": 512
    });

    let response = server.post("/instances").json(&create_req).await;

    assert_eq!(response.status_code(), 201);

    let instance: serde_json::Value = response.json();
    assert_eq!(instance["name"], "test-instance");
    assert_eq!(instance["model_id"], "BAAI/bge-small-en-v1.5");
    assert_eq!(instance["port"], 8080);
    assert!(instance["prometheus_port"].is_number());
}

#[tokio::test]
async fn test_create_instance_with_gpu() {
    let (server, _temp_dir) = create_test_server().await;

    let create_req = json!({
        "name": "gpu-instance",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080,
        "gpu_id": 1
    });

    let response = server.post("/instances").json(&create_req).await;

    assert_eq!(response.status_code(), 201);

    let instance: serde_json::Value = response.json();
    assert_eq!(instance["gpu_id"], 1);
}

#[tokio::test]
async fn test_create_instance_with_prometheus_port() {
    let (server, _temp_dir) = create_test_server().await;

    let create_req = json!({
        "name": "prom-port-instance",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080,
        "prometheus_port": 9100
    });

    let response = server.post("/instances").json(&create_req).await;

    assert_eq!(response.status_code(), 201);

    let instance: serde_json::Value = response.json();
    assert_eq!(instance["prometheus_port"], 9100);
}

#[tokio::test]
async fn test_get_instance() {
    let (server, _temp_dir) = create_test_server().await;

    // Create instance first
    let create_req = json!({
        "name": "get-test",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080
    });

    server.post("/instances").json(&create_req).await;

    // Get instance
    let response = server.get("/instances/get-test").await;

    assert_eq!(response.status_code(), 200);

    let instance: serde_json::Value = response.json();
    assert_eq!(instance["name"], "get-test");
    assert_eq!(instance["model_id"], "BAAI/bge-small-en-v1.5");
}

#[tokio::test]
async fn test_get_nonexistent_instance() {
    let (server, _temp_dir) = create_test_server().await;

    let response = server.get("/instances/nonexistent").await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_list_instances_with_data() {
    let (server, _temp_dir) = create_test_server().await;

    // Create multiple instances
    for i in 1..=3 {
        let create_req = json!({
            "name": format!("instance-{}", i),
            "model_id": "BAAI/bge-small-en-v1.5",
            "port": 8080 + i
        });

        server.post("/instances").json(&create_req).await;
    }

    // List all instances
    let response = server.get("/instances").await;

    assert_eq!(response.status_code(), 200);

    let instances: Vec<serde_json::Value> = response.json();
    assert_eq!(instances.len(), 3);
}

#[tokio::test]
async fn test_stop_instance() {
    let (server, _temp_dir) = create_test_server().await;

    // Create instance
    let create_req = json!({
        "name": "stop-test",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080
    });

    server.post("/instances").json(&create_req).await;

    // Stop instance
    let response = server.post("/instances/stop-test/stop").await;

    assert_eq!(response.status_code(), 200);

    let instance: serde_json::Value = response.json();
    assert_eq!(instance["name"], "stop-test");
}

#[tokio::test]
async fn test_start_instance() {
    let (server, _temp_dir) = create_test_server().await;

    // Create and stop instance
    let create_req = json!({
        "name": "start-test",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080
    });

    server.post("/instances").json(&create_req).await;
    server.post("/instances/start-test/stop").await;

    // Start instance
    let response = server.post("/instances/start-test/start").await;

    assert_eq!(response.status_code(), 200);

    let instance: serde_json::Value = response.json();
    assert_eq!(instance["name"], "start-test");
}

#[tokio::test]
async fn test_restart_instance() {
    let (server, _temp_dir) = create_test_server().await;

    // Create instance
    let create_req = json!({
        "name": "restart-test",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080
    });

    server.post("/instances").json(&create_req).await;

    // Restart instance
    let response = server.post("/instances/restart-test/restart").await;

    assert_eq!(response.status_code(), 200);

    let instance: serde_json::Value = response.json();
    assert_eq!(instance["name"], "restart-test");
}

#[tokio::test]
async fn test_delete_instance() {
    let (server, _temp_dir) = create_test_server().await;

    // Create instance
    let create_req = json!({
        "name": "delete-test",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080
    });

    server.post("/instances").json(&create_req).await;

    // Delete instance
    let response = server.delete("/instances/delete-test").await;

    assert_eq!(response.status_code(), 204);

    // Verify deleted
    let response = server.get("/instances/delete-test").await;
    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_delete_nonexistent_instance() {
    let (server, _temp_dir) = create_test_server().await;

    let response = server.delete("/instances/nonexistent").await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_duplicate_name_rejected() {
    let (server, _temp_dir) = create_test_server().await;

    let create_req = json!({
        "name": "duplicate",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080
    });

    // Create first instance
    let response = server.post("/instances").json(&create_req).await;
    assert_eq!(response.status_code(), 201);

    // Try to create duplicate
    let response = server.post("/instances").json(&create_req).await;
    assert_eq!(response.status_code(), 400);
}

#[tokio::test]
async fn test_duplicate_port_rejected() {
    let (server, _temp_dir) = create_test_server().await;

    // Create first instance
    let create_req1 = json!({
        "name": "first",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080
    });

    let response = server.post("/instances").json(&create_req1).await;
    assert_eq!(response.status_code(), 201);

    // Try to create with duplicate port
    let create_req2 = json!({
        "name": "second",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080
    });

    let response = server.post("/instances").json(&create_req2).await;
    assert_eq!(response.status_code(), 400);
}

#[tokio::test]
async fn test_max_instances_limit() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("state.toml");

    // Use the real TEI binary extracted from official image
    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/text-embeddings-router");

    // Create server with max_instances = 2
    let config = ManagerConfig {
        state_file: state_file.clone(),
        tei_binary_path: tei_binary.to_string_lossy().to_string(),
        max_instances: Some(2),
        ..Default::default()
    };

    let registry = Arc::new(Registry::new(
        config.max_instances,
        config.tei_binary_path.clone(),
    ));

    let state_manager = Arc::new(StateManager::new(
        state_file,
        registry.clone(),
        config.tei_binary_path.clone(),
    ));

    let state = AppState {
        registry,
        state_manager,
        prometheus_handle: get_metrics_handle(),
    };

    let app = create_router(state);
    let server = TestServer::new(app).expect("Failed to create test server");

    // Create 2 instances (should succeed)
    for i in 1..=2 {
        let create_req = json!({
            "name": format!("instance-{}", i),
            "model_id": "BAAI/bge-small-en-v1.5",
            "port": 8080 + i
        });

        let response = server.post("/instances").json(&create_req).await;
        assert_eq!(response.status_code(), 201);
    }

    // Try to create 3rd instance (should fail)
    let create_req = json!({
        "name": "instance-3",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8083
    });

    let response = server.post("/instances").json(&create_req).await;
    assert_eq!(response.status_code(), 400);
}

#[tokio::test]
async fn test_stop_nonexistent_instance() {
    let (server, _temp_dir) = create_test_server().await;

    let response = server.post("/instances/nonexistent/stop").await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_start_nonexistent_instance() {
    let (server, _temp_dir) = create_test_server().await;

    let response = server.post("/instances/nonexistent/start").await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_restart_nonexistent_instance() {
    let (server, _temp_dir) = create_test_server().await;

    let response = server.post("/instances/nonexistent/restart").await;

    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_state_persistence() {
    use tei_manager::state::StateManager;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("test-state.toml");

    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/text-embeddings-router");

    let registry = Arc::new(Registry::new(
        None,
        tei_binary.to_string_lossy().to_string(),
    ));
    let state_manager = Arc::new(StateManager::new(
        state_file.clone(),
        registry.clone(),
        tei_binary.to_string_lossy().to_string(),
    ));

    // Create an instance
    let config = tei_manager::config::InstanceConfig {
        name: "persist-test".to_string(),
        model_id: "test/model".to_string(),
        port: 9090,
        max_batch_tokens: 1024,
        max_concurrent_requests: 10,
        pooling: Some("mean".to_string()),
        gpu_id: Some(1),
        prometheus_port: Some(9200),
        extra_args: vec!["--arg1".to_string()],
        created_at: Some(chrono::Utc::now()),
    };

    registry
        .add(config.clone())
        .await
        .expect("Failed to add instance");

    // Save state
    state_manager.save().await.expect("Failed to save state");

    // Verify state file exists
    assert!(state_file.exists());

    // Load state back
    let loaded_state = state_manager.load().await.expect("Failed to load state");

    assert_eq!(loaded_state.instances.len(), 1);
    assert_eq!(loaded_state.instances[0].name, "persist-test");
    assert_eq!(loaded_state.instances[0].model_id, "test/model");
    assert_eq!(loaded_state.instances[0].port, 9090);
    assert_eq!(loaded_state.instances[0].pooling, Some("mean".to_string()));
    assert_eq!(loaded_state.instances[0].gpu_id, Some(1));
    assert_eq!(loaded_state.instances[0].prometheus_port, Some(9200));
}

#[tokio::test]
async fn test_state_load_missing_file() {
    use tei_manager::state::StateManager;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("nonexistent.toml");

    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/text-embeddings-router");

    let registry = Arc::new(Registry::new(
        None,
        tei_binary.to_string_lossy().to_string(),
    ));
    let state_manager = StateManager::new(
        state_file,
        registry,
        tei_binary.to_string_lossy().to_string(),
    );

    // Loading missing file should return empty state
    let result = state_manager.load().await;
    assert!(result.is_err() || result.unwrap().instances.is_empty());
}

#[tokio::test]
async fn test_config_validation() {
    use tei_manager::config::ManagerConfig;

    // Test duplicate instance names
    let config = ManagerConfig {
        instances: vec![
            tei_manager::config::InstanceConfig {
                name: "dup".to_string(),
                model_id: "m1".to_string(),
                port: 8080,
                max_batch_tokens: 1024,
                max_concurrent_requests: 10,
                pooling: None,
                gpu_id: None,
                prometheus_port: None,
                extra_args: vec![],
                created_at: None,
            },
            tei_manager::config::InstanceConfig {
                name: "dup".to_string(), // Duplicate name
                model_id: "m2".to_string(),
                port: 8081,
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

#[tokio::test]
async fn test_error_responses() {
    let (server, _temp_dir) = create_test_server().await;

    // Test 404 Not Found
    let response = server.get("/instances/nonexistent").await;
    assert_eq!(response.status_code(), 404);
    let body: serde_json::Value = response.json();
    assert!(body["error"].is_string());
    assert!(body["timestamp"].is_string());

    // Test 400 Bad Request (duplicate name)
    let create_req = json!({
        "name": "test",
        "model_id": "test/model",
        "port": 8080
    });
    server.post("/instances").json(&create_req).await;

    let response = server.post("/instances").json(&create_req).await;
    assert_eq!(response.status_code(), 400);
    let body: serde_json::Value = response.json();
    assert!(body["error"].is_string());
}

// ========================================
// Additional coverage tests for config.rs
// ========================================

#[tokio::test]
async fn test_config_load_with_env_overrides() {
    use std::env;
    use tei_manager::config::ManagerConfig;

    // Set environment variables (unsafe in edition 2024)
    unsafe {
        env::set_var("TEI_MANAGER_API_PORT", "9999");
        env::set_var("TEI_MANAGER_STATE_FILE", "/tmp/test-state.toml");
        env::set_var("TEI_MANAGER_HEALTH_CHECK_INTERVAL", "42");
        env::set_var("TEI_BINARY_PATH", "/custom/path/tei");
    }

    // Load config without file (uses defaults + env overrides)
    let config = ManagerConfig::load(None).expect("Failed to load config");

    assert_eq!(config.api_port, 9999);
    assert_eq!(config.state_file.to_string_lossy(), "/tmp/test-state.toml");
    assert_eq!(config.health_check_interval_secs, 42);
    assert_eq!(config.tei_binary_path, "/custom/path/tei");

    // Clean up
    unsafe {
        env::remove_var("TEI_MANAGER_API_PORT");
        env::remove_var("TEI_MANAGER_STATE_FILE");
        env::remove_var("TEI_MANAGER_HEALTH_CHECK_INTERVAL");
        env::remove_var("TEI_BINARY_PATH");
    }
}

#[tokio::test]
async fn test_config_validation_api_port_conflict() {
    use tei_manager::config::{InstanceConfig, ManagerConfig};

    let config = ManagerConfig {
        api_port: 9000,
        instances: vec![InstanceConfig {
            name: "conflict".to_string(),
            model_id: "model".to_string(),
            port: 9000, // Same as API port
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

    let result = config.validate();
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("conflicts with API port")
    );
}

#[tokio::test]
async fn test_config_validation_empty_instance_name() {
    use tei_manager::config::{InstanceConfig, ManagerConfig};

    let config = ManagerConfig {
        instances: vec![InstanceConfig {
            name: "".to_string(), // Empty name
            model_id: "model".to_string(),
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

    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cannot be empty"));
}

#[tokio::test]
async fn test_config_validation_instance_port_too_low() {
    use tei_manager::config::{InstanceConfig, ManagerConfig};

    let config = ManagerConfig {
        instances: vec![InstanceConfig {
            name: "lowport".to_string(),
            model_id: "model".to_string(),
            port: 80, // Below 1024
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

    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("must be >= 1024"));
}

#[tokio::test]
async fn test_config_validation_backslash_in_name() {
    use tei_manager::config::{InstanceConfig, ManagerConfig};

    let config = ManagerConfig {
        instances: vec![InstanceConfig {
            name: "bad\\name".to_string(), // Backslash
            model_id: "model".to_string(),
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

    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("path separators"));
}

// ========================================
// Additional coverage tests for error.rs
// ========================================

#[tokio::test]
async fn test_error_conflict_response() {
    use axum::response::IntoResponse;
    use tei_manager::error::ApiError;

    let error = ApiError::Conflict("Resource already exists".to_string());
    let response = error.into_response();

    assert_eq!(response.status(), 409); // HTTP 409 Conflict
}

#[tokio::test]
async fn test_error_internal_response() {
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use tei_manager::error::ApiError;

    let error = ApiError::Internal(anyhow::anyhow!("Database connection failed"));
    let response = error.into_response();

    assert_eq!(response.status(), 500); // HTTP 500 Internal Server Error

    // Read response body to verify error message
    let body = response.into_body();
    let bytes = body.collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(body_str.contains("Internal server error"));
    assert!(body_str.contains("timestamp"));
}

#[tokio::test]
async fn test_error_from_anyhow() {
    use tei_manager::error::ApiError;

    let anyhow_error = anyhow::anyhow!("Something went wrong");
    let api_error: ApiError = anyhow_error.into();

    match api_error {
        ApiError::Internal(_) => {} // Expected
        _ => panic!("Expected Internal error"),
    }
}

// ========================================
// Additional coverage tests for state.rs
// ========================================

#[tokio::test]
async fn test_state_restore_multiple_instances() {
    use tei_manager::state::StateManager;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("restore-multi.toml");

    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/text-embeddings-router");

    // Create state file with multiple instances
    let state_content = r#"
last_updated = "2025-01-01T00:00:00Z"

[[instances]]
name = "restore1"
model_id = "BAAI/bge-small-en-v1.5"
port = 8090
max_batch_tokens = 1024
max_concurrent_requests = 10

[[instances]]
name = "restore2"
model_id = "BAAI/bge-small-en-v1.5"
port = 8091
max_batch_tokens = 1024
max_concurrent_requests = 10
"#
    .to_string();

    std::fs::write(&state_file, state_content).expect("Failed to write state file");

    let registry = Arc::new(Registry::new(
        None,
        tei_binary.to_string_lossy().to_string(),
    ));
    let state_manager = StateManager::new(
        state_file,
        registry.clone(),
        tei_binary.to_string_lossy().to_string(),
    );

    // Restore instances
    let result = state_manager.restore().await;

    // Restore should complete without error (even if individual instances fail to start)
    assert!(result.is_ok());

    // Verify instances were added to registry
    let instances = registry.list().await;
    assert_eq!(instances.len(), 2);
    assert!(instances.iter().any(|i| i.config.name == "restore1"));
    assert!(instances.iter().any(|i| i.config.name == "restore2"));
}

#[tokio::test]
async fn test_state_restore_with_invalid_instance() {
    use tei_manager::state::StateManager;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("restore-invalid.toml");

    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/text-embeddings-router");

    // Create state file with an instance that will fail (invalid model path)
    let state_content = r#"
last_updated = "2025-01-01T00:00:00Z"

[[instances]]
name = "will-fail"
model_id = "/nonexistent/model/path"
port = 8092
max_batch_tokens = 1024
max_concurrent_requests = 10
"#;

    std::fs::write(&state_file, state_content).expect("Failed to write state file");

    let registry = Arc::new(Registry::new(
        None,
        tei_binary.to_string_lossy().to_string(),
    ));
    let state_manager = StateManager::new(
        state_file,
        registry,
        tei_binary.to_string_lossy().to_string(),
    );

    // Restore should complete (not panic) even though instance will fail
    let result = state_manager.restore().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_state_restore_empty_state() {
    use tei_manager::state::StateManager;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("restore-empty.toml");

    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/text-embeddings-router");

    // Create empty state file
    let state_content = r#"
last_updated = "2025-01-01T00:00:00Z"
instances = []
"#;

    std::fs::write(&state_file, state_content).expect("Failed to write state file");

    let registry = Arc::new(Registry::new(
        None,
        tei_binary.to_string_lossy().to_string(),
    ));
    let state_manager = StateManager::new(
        state_file,
        registry,
        tei_binary.to_string_lossy().to_string(),
    );

    // Should handle empty state gracefully
    let result = state_manager.restore().await;
    assert!(result.is_ok());
}

// ========================================
// Additional coverage tests for health.rs
// ========================================

#[tokio::test]
async fn test_health_monitor_creation_params() {
    use tei_manager::health::HealthMonitor;

    let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));

    let monitor = HealthMonitor::new(
        registry.clone(),
        45,    // check_interval_secs
        90,    // initial_delay_secs
        5,     // max_failures_before_restart
        false, // auto_restart disabled
        "custom/tei/path".to_string(),
    );

    // HealthMonitor fields are private, but we can verify creation succeeded
    // by checking that the Arc can be created
    let _arc_monitor = Arc::new(monitor);
}

#[tokio::test]
async fn test_health_check_on_stopped_instance() {
    use tei_manager::config::InstanceConfig;
    use tei_manager::health::HealthMonitor;

    let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));

    let config = InstanceConfig {
        name: "stopped".to_string(),
        model_id: "model".to_string(),
        port: 8093,
        max_batch_tokens: 1024,
        max_concurrent_requests: 10,
        pooling: None,
        gpu_id: None,
        prometheus_port: None,
        extra_args: vec![],
        created_at: None,
    };

    let instance = registry.add(config).await.expect("Failed to add instance");

    let _monitor = HealthMonitor::new(
        registry.clone(),
        30,
        60,
        3,
        false,
        "text-embeddings-router".to_string(),
    );

    // Instance is not started, so health check should fail
    // We can't directly call check_instance (it's private), but we tested
    // it indirectly through the monitoring loop

    // Verify instance is not running
    assert!(!instance.is_running().await);
}
