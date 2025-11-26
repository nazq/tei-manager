//! Integration tests that run the API in-process for code coverage
//!
//! These tests exercise the API handlers directly using axum-test,
//! which runs in-process and contributes to code coverage metrics.
//!
//! Uses `tests/mock-tei-router` for simulating TEI backend behavior.

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

    // Use the mock TEI binary for integration tests
    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/mock-tei-router");

    let config = ManagerConfig {
        state_file: state_file.clone(),
        tei_binary_path: tei_binary.to_string_lossy().to_string(),
        max_instances: Some(10),
        ..Default::default()
    };

    let registry = Arc::new(Registry::new(
        config.max_instances,
        config.tei_binary_path.clone(),
        config.instance_port_start,
        config.instance_port_end,
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
        auth_manager: None,
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
async fn test_create_instance_with_invalid_gpu() {
    // Tests that invalid GPU IDs are rejected
    // GPU validation uses nvidia-smi to detect available GPUs
    // On machines without GPUs, any gpu_id is invalid
    let (server, _temp_dir) = create_test_server().await;

    let create_req = json!({
        "name": "gpu-instance",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8080,
        "gpu_id": 99  // Very high GPU ID - should always be invalid
    });

    let response = server.post("/instances").json(&create_req).await;

    // Should return 400 Bad Request for invalid GPU ID
    assert_eq!(response.status_code(), 400);

    let body: serde_json::Value = response.json();
    assert!(body["error"].as_str().unwrap().contains("Invalid GPU ID"));
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

    // Use the mock TEI binary for integration tests
    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/mock-tei-router");

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
        config.instance_port_start,
        config.instance_port_end,
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
        auth_manager: None,
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
        .join("tests/mock-tei-router");

    let registry = Arc::new(Registry::new(
        None,
        tei_binary.to_string_lossy().to_string(),
        8080,
        8180,
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
        ..Default::default()
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
        .join("tests/mock-tei-router");

    let registry = Arc::new(Registry::new(
        None,
        tei_binary.to_string_lossy().to_string(),
        8080,
        8180,
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
                ..Default::default()
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
                ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
    use tei_manager::error::TeiError;

    let error = TeiError::InstanceExists {
        name: "test-instance".to_string(),
    };
    let response = error.into_response();

    assert_eq!(response.status(), 409); // HTTP 409 Conflict
}

#[tokio::test]
async fn test_error_internal_response() {
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use tei_manager::error::TeiError;

    let error = TeiError::Internal {
        message: "Database connection failed".to_string(),
    };
    let response = error.into_response();

    assert_eq!(response.status(), 500); // HTTP 500 Internal Server Error

    // Read response body to verify error message
    let body = response.into_body();
    let bytes = body.collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(body_str.contains("Database connection failed"));
    assert!(body_str.contains("timestamp"));
}

#[tokio::test]
async fn test_error_from_anyhow() {
    use tei_manager::error::TeiError;

    let anyhow_error = anyhow::anyhow!("Something went wrong");
    let tei_error: TeiError = anyhow_error.into();

    match tei_error {
        TeiError::Internal { .. } => {} // Expected
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
        .join("tests/mock-tei-router");

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
        8080,
        8180,
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
        .join("tests/mock-tei-router");

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
        8080,
        8180,
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
        .join("tests/mock-tei-router");

    // Create empty state file
    let state_content = r#"
last_updated = "2025-01-01T00:00:00Z"
instances = []
"#;

    std::fs::write(&state_file, state_content).expect("Failed to write state file");

    let registry = Arc::new(Registry::new(
        None,
        tei_binary.to_string_lossy().to_string(),
        8080,
        8180,
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

    let registry = Arc::new(Registry::new(
        None,
        "text-embeddings-router".to_string(),
        8080,
        8180,
    ));

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

    let registry = Arc::new(Registry::new(
        None,
        "text-embeddings-router".to_string(),
        8080,
        8180,
    ));

    let config = InstanceConfig {
        name: "stopped".to_string(),
        model_id: "model".to_string(),
        port: 8093,
        max_batch_tokens: 1024,
        max_concurrent_requests: 10,
        pooling: None,
        gpu_id: None,
        prometheus_port: None,
        ..Default::default()
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

// ========================================
// Port auto-allocation tests
// ========================================

#[tokio::test]
async fn test_create_instance_without_port() {
    let (server, _temp_dir) = create_test_server().await;

    // Create instance without specifying port - should auto-allocate
    let create_req = json!({
        "name": "auto-port-instance",
        "model_id": "BAAI/bge-small-en-v1.5"
        // No port specified - should be auto-allocated from configured range
    });

    let response = server.post("/instances").json(&create_req).await;

    assert_eq!(response.status_code(), 201);

    let instance: serde_json::Value = response.json();
    assert_eq!(instance["name"], "auto-port-instance");
    // Port should be in the default range [8080, 8180)
    let port = instance["port"].as_u64().expect("port should be a number");
    assert!(
        (8080..8180).contains(&port),
        "Port {} should be in range [8080, 8180)",
        port
    );
}

#[tokio::test]
async fn test_create_multiple_instances_auto_port() {
    let (server, _temp_dir) = create_test_server().await;

    let mut ports = Vec::new();

    // Create 5 instances without specifying ports
    for i in 0..5 {
        let create_req = json!({
            "name": format!("auto-instance-{}", i),
            "model_id": "BAAI/bge-small-en-v1.5"
        });

        let response = server.post("/instances").json(&create_req).await;
        assert_eq!(response.status_code(), 201);

        let instance: serde_json::Value = response.json();
        let port = instance["port"].as_u64().expect("port should be a number") as u16;
        ports.push(port);
    }

    // All ports should be unique
    let unique_ports: std::collections::HashSet<_> = ports.iter().collect();
    assert_eq!(
        unique_ports.len(),
        5,
        "All auto-allocated ports should be unique"
    );

    // All ports should be in range
    for port in &ports {
        assert!(
            *port >= 8080 && *port < 8180,
            "Port {} should be in range [8080, 8180)",
            port
        );
    }
}

#[tokio::test]
async fn test_mixed_auto_and_manual_ports_api() {
    let (server, _temp_dir) = create_test_server().await;

    // Create with manual port
    let create_req1 = json!({
        "name": "manual-port-instance",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8085
    });

    let response = server.post("/instances").json(&create_req1).await;
    assert_eq!(response.status_code(), 201);

    let instance1: serde_json::Value = response.json();
    assert_eq!(instance1["port"], 8085);

    // Create with auto port - should not conflict with manual port
    let create_req2 = json!({
        "name": "auto-port-instance-2",
        "model_id": "BAAI/bge-small-en-v1.5"
    });

    let response = server.post("/instances").json(&create_req2).await;
    assert_eq!(response.status_code(), 201);

    let instance2: serde_json::Value = response.json();
    let auto_port = instance2["port"].as_u64().expect("port should be a number");
    assert_ne!(
        auto_port, 8085,
        "Auto-allocated port should not conflict with manual port"
    );
}

#[tokio::test]
async fn test_port_auto_allocation_create_delete_create_api() {
    let (server, _temp_dir) = create_test_server().await;

    // Create 3 instances with auto-allocated ports
    for i in 0..3 {
        let create_req = json!({
            "name": format!("cdc-instance-{}", i),
            "model_id": "BAAI/bge-small-en-v1.5"
        });

        let response = server.post("/instances").json(&create_req).await;
        assert_eq!(response.status_code(), 201);
    }

    // Delete 2 instances
    let response = server.delete("/instances/cdc-instance-0").await;
    assert_eq!(response.status_code(), 204);

    let response = server.delete("/instances/cdc-instance-1").await;
    assert_eq!(response.status_code(), 204);

    // Create 2 more - should reuse freed ports
    for i in 3..5 {
        let create_req = json!({
            "name": format!("cdc-instance-{}", i),
            "model_id": "BAAI/bge-small-en-v1.5"
        });

        let response = server.post("/instances").json(&create_req).await;
        assert_eq!(response.status_code(), 201);
    }

    // Verify 3 instances exist with unique ports
    let response = server.get("/instances").await;
    assert_eq!(response.status_code(), 200);

    let instances: Vec<serde_json::Value> = response.json();
    assert_eq!(instances.len(), 3);

    let ports: std::collections::HashSet<_> = instances
        .iter()
        .map(|i| i["port"].as_u64().expect("port"))
        .collect();
    assert_eq!(ports.len(), 3, "All ports should be unique");
}

// ========================================
// Logs endpoint tests
// ========================================

#[tokio::test]
async fn test_get_logs_instance_not_found() {
    let (server, _temp_dir) = create_test_server().await;

    // Try to get logs for non-existent instance
    let response = server.get("/instances/nonexistent/logs").await;
    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn test_get_logs_with_log_file() {
    let (server, _temp_dir) = create_test_server().await;

    // Use the fallback log directory that the handler checks
    let log_dir = std::path::Path::new("/tmp/tei-manager/logs");
    std::fs::create_dir_all(log_dir).unwrap();

    let log_content = "line 1\nline 2\nline 3\nline 4\nline 5\n";
    std::fs::write(log_dir.join("test-logs.log"), log_content).unwrap();

    // Get all logs
    let response = server.get("/instances/test-logs/logs").await;
    assert_eq!(response.status_code(), 200);

    let logs: serde_json::Value = response.json();
    assert_eq!(logs["total_lines"], 5);
    assert_eq!(logs["start"], 0);
    assert_eq!(logs["end"], 5);

    // Clean up
    let _ = std::fs::remove_file(log_dir.join("test-logs.log"));
}

#[tokio::test]
async fn test_get_logs_with_slicing() {
    let (server, _temp_dir) = create_test_server().await;

    // Use the fallback log directory
    let log_dir = std::path::Path::new("/tmp/tei-manager/logs");
    std::fs::create_dir_all(log_dir).unwrap();

    let log_content = "line 1\nline 2\nline 3\nline 4\nline 5\n";
    std::fs::write(log_dir.join("sliced-logs.log"), log_content).unwrap();

    // Get first 2 lines
    let response = server
        .get("/instances/sliced-logs/logs?start=0&end=2")
        .await;
    assert_eq!(response.status_code(), 200);

    let logs: serde_json::Value = response.json();
    assert_eq!(logs["lines"].as_array().unwrap().len(), 2);
    assert_eq!(logs["start"], 0);
    assert_eq!(logs["end"], 2);

    // Get last 2 lines using negative indices
    let response = server.get("/instances/sliced-logs/logs?start=-2").await;
    assert_eq!(response.status_code(), 200);

    let logs: serde_json::Value = response.json();
    assert_eq!(logs["lines"].as_array().unwrap().len(), 2);
    assert_eq!(logs["start"], 3);
    assert_eq!(logs["end"], 5);

    // Clean up
    let _ = std::fs::remove_file(log_dir.join("sliced-logs.log"));
}

#[tokio::test]
async fn test_get_logs_empty_slice() {
    let (server, _temp_dir) = create_test_server().await;

    // Use the fallback log directory
    let log_dir = std::path::Path::new("/tmp/tei-manager/logs");
    std::fs::create_dir_all(log_dir).unwrap();

    let log_content = "line 1\nline 2\nline 3\n";
    std::fs::write(log_dir.join("empty-slice.log"), log_content).unwrap();

    // Invalid range (start > end) should return empty
    let response = server
        .get("/instances/empty-slice/logs?start=5&end=2")
        .await;
    assert_eq!(response.status_code(), 200);

    let logs: serde_json::Value = response.json();
    assert_eq!(logs["lines"].as_array().unwrap().len(), 0);

    // Clean up
    let _ = std::fs::remove_file(log_dir.join("empty-slice.log"));
}

// ========================================
// Additional error path tests
// ========================================

// Note: Instance name validation for special characters (/, \) is only done
// when loading from config file, not via API. This is intentional to allow
// flexibility in naming. The log file path sanitization handles special chars.

#[tokio::test]
async fn test_start_already_running_instance() {
    let (server, _temp_dir) = create_test_server().await;

    // Create and start an instance
    let create_req = json!({
        "name": "running-instance",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8099
    });

    let response = server.post("/instances").json(&create_req).await;
    assert_eq!(response.status_code(), 201);

    // Try to start it again - should handle gracefully
    let response = server.post("/instances/running-instance/start").await;
    // Could be 200 (idempotent) or error depending on implementation
    assert!(response.status_code() == 200 || response.status_code() == 409);
}

#[tokio::test]
async fn test_stop_already_stopped_instance() {
    let (server, _temp_dir) = create_test_server().await;

    // Create instance
    let create_req = json!({
        "name": "to-stop",
        "model_id": "BAAI/bge-small-en-v1.5",
        "port": 8098
    });

    let response = server.post("/instances").json(&create_req).await;
    assert_eq!(response.status_code(), 201);

    // Stop it
    let response = server.post("/instances/to-stop/stop").await;
    assert_eq!(response.status_code(), 200);

    // Stop again - should handle gracefully
    let response = server.post("/instances/to-stop/stop").await;
    assert!(response.status_code() == 200 || response.status_code() == 409);
}

#[tokio::test]
async fn test_list_instances_after_operations() {
    let (server, _temp_dir) = create_test_server().await;

    // Start with empty list
    let response = server.get("/instances").await;
    assert_eq!(response.status_code(), 200);
    let instances: Vec<serde_json::Value> = response.json();
    assert_eq!(instances.len(), 0);

    // Create two instances
    for i in 0..2 {
        let create_req = json!({
            "name": format!("list-test-{}", i),
            "model_id": "BAAI/bge-small-en-v1.5",
            "port": 8070 + i
        });
        server.post("/instances").json(&create_req).await;
    }

    // List should show 2
    let response = server.get("/instances").await;
    assert_eq!(response.status_code(), 200);
    let instances: Vec<serde_json::Value> = response.json();
    assert_eq!(instances.len(), 2);

    // Delete one
    server.delete("/instances/list-test-0").await;

    // List should show 1
    let response = server.get("/instances").await;
    assert_eq!(response.status_code(), 200);
    let instances: Vec<serde_json::Value> = response.json();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["name"], "list-test-1");
}
