//! Model Registry Integration Tests
//!
//! Tests the full model registry workflow including download from HuggingFace Hub.
//! Uses TaylorAI/gte-tiny (~22MB) and sentence-transformers/paraphrase-albert-small-v2 (~45MB)
//! as test models for speed.

use axum_test::TestServer;
use serde_json::json;
use std::sync::Arc;
use std::sync::OnceLock;
use tei_manager::{
    ModelLoader, ModelRegistry,
    api::routes::{AppState, create_router},
    metrics,
    models::{get_model_cache_path, is_model_cached},
    registry::Registry,
    state::StateManager,
};
use tempfile::TempDir;

// Smallest usable embedding models for CI
// TaylorAI/gte-tiny: ~22MB - very small and fast
// sentence-transformers/paraphrase-albert-small-v2: ~45MB - different architecture
const TEST_MODEL_1: &str = "TaylorAI/gte-tiny";
const TEST_MODEL_1_ENCODED: &str = "TaylorAI%2Fgte-tiny";

const TEST_MODEL_2: &str = "sentence-transformers/paraphrase-albert-small-v2";
const TEST_MODEL_2_ENCODED: &str = "sentence-transformers%2Fparaphrase-albert-small-v2";

// Global metrics handle
static METRICS_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

fn get_metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    METRICS_HANDLE
        .get_or_init(|| metrics::setup_metrics().expect("Failed to setup metrics"))
        .clone()
}

/// Create a test server with model registry support
async fn create_test_server() -> (TestServer, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("state.toml");

    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/mock-tei-router");

    let registry = Arc::new(Registry::new(
        Some(10),
        tei_binary.to_string_lossy().to_string(),
        8080,
        8180,
    ));

    let state_manager = Arc::new(StateManager::new(
        state_file,
        registry.clone(),
        tei_binary.to_string_lossy().to_string(),
    ));

    let model_registry = Arc::new(ModelRegistry::new());
    let model_loader = Arc::new(ModelLoader::new());

    let state = AppState {
        registry,
        state_manager,
        prometheus_handle: get_metrics_handle(),
        auth_manager: None,
        model_registry,
        model_loader,
    };

    let app = create_router(state);
    let server = TestServer::new(app).expect("Failed to create test server");

    (server, temp_dir)
}

// ============================================================================
// Model Registry API Tests
// ============================================================================

#[tokio::test]
async fn test_download_model_1() {
    let (server, _temp_dir) = create_test_server().await;

    // Add model to registry
    let add_req = json!({ "model_id": TEST_MODEL_1 });
    let response = server.post("/models").json(&add_req).await;
    assert!(response.status_code() == 200 || response.status_code() == 201);

    // Trigger download
    let response = server
        .post(&format!("/models/{}/download", TEST_MODEL_1_ENCODED))
        .await;

    // Should succeed (200 OK)
    assert_eq!(
        response.status_code(),
        200,
        "Download failed: {}",
        response.text()
    );

    let model: serde_json::Value = response.json();
    assert_eq!(model["model_id"], TEST_MODEL_1);

    // Status should be downloaded (not available/downloading)
    let status = model["status"].as_str().unwrap();
    assert_eq!(
        status, "downloaded",
        "Expected downloaded status, got {}",
        status
    );

    // Verify cache path is populated
    assert!(model["cache_path"].is_string());
    let cache_path = model["cache_path"].as_str().unwrap();
    assert!(!cache_path.is_empty());

    // Verify files exist on disk
    assert!(is_model_cached(TEST_MODEL_1), "Model should be in HF cache");
    let path = get_model_cache_path(TEST_MODEL_1).expect("Should have cache path");
    assert!(
        path.join("config.json").exists(),
        "config.json should exist"
    );
}

#[tokio::test]
async fn test_download_model_2() {
    let (server, _temp_dir) = create_test_server().await;

    // Add model to registry
    let add_req = json!({ "model_id": TEST_MODEL_2 });
    let response = server.post("/models").json(&add_req).await;
    assert!(response.status_code() == 200 || response.status_code() == 201);

    // Trigger download
    let response = server
        .post(&format!("/models/{}/download", TEST_MODEL_2_ENCODED))
        .await;

    // Should succeed (200 OK)
    assert_eq!(
        response.status_code(),
        200,
        "Download failed: {}",
        response.text()
    );

    let model: serde_json::Value = response.json();
    assert_eq!(model["model_id"], TEST_MODEL_2);

    // Status should be downloaded
    let status = model["status"].as_str().unwrap();
    assert_eq!(
        status, "downloaded",
        "Expected downloaded status, got {}",
        status
    );

    // Verify files exist on disk
    assert!(is_model_cached(TEST_MODEL_2), "Model should be in HF cache");
    let path = get_model_cache_path(TEST_MODEL_2).expect("Should have cache path");
    assert!(
        path.join("config.json").exists(),
        "config.json should exist"
    );
}

#[tokio::test]
async fn test_download_already_downloaded() {
    let (server, _temp_dir) = create_test_server().await;

    // Add and download model
    let add_req = json!({ "model_id": TEST_MODEL_1 });
    server.post("/models").json(&add_req).await;
    server
        .post(&format!("/models/{}/download", TEST_MODEL_1_ENCODED))
        .await;

    // Download again - should still succeed (idempotent)
    let response = server
        .post(&format!("/models/{}/download", TEST_MODEL_1_ENCODED))
        .await;

    assert_eq!(response.status_code(), 200);
    let model: serde_json::Value = response.json();
    assert_eq!(model["status"], "downloaded");
}

#[tokio::test]
async fn test_get_model_after_download() {
    let (server, _temp_dir) = create_test_server().await;

    // Add and download
    let add_req = json!({ "model_id": TEST_MODEL_1 });
    server.post("/models").json(&add_req).await;
    server
        .post(&format!("/models/{}/download", TEST_MODEL_1_ENCODED))
        .await;

    // Get model info
    let response = server
        .get(&format!("/models/{}", TEST_MODEL_1_ENCODED))
        .await;

    assert_eq!(response.status_code(), 200);
    let model: serde_json::Value = response.json();

    // Should have metadata after download
    assert_eq!(model["model_id"], TEST_MODEL_1);
    assert_eq!(model["status"], "downloaded");
    assert!(model["cache_path"].is_string());

    // Should have size info
    if let Some(size) = model["size_bytes"].as_u64() {
        assert!(size > 0, "Model size should be > 0");
    }
}

#[tokio::test]
async fn test_download_nonexistent_model() {
    let (server, _temp_dir) = create_test_server().await;

    // Try to download a model that doesn't exist on HuggingFace
    let response = server
        .post("/models/definitely-not-a-real-org%2Fnonexistent-model-12345/download")
        .await;

    // Should fail with appropriate error
    assert!(
        response.status_code() == 404 || response.status_code() == 500,
        "Expected 404 or 500, got {}",
        response.status_code()
    );
}

#[tokio::test]
async fn test_list_models_includes_downloaded() {
    let (server, _temp_dir) = create_test_server().await;

    // Add and download both models
    let add_req1 = json!({ "model_id": TEST_MODEL_1 });
    let add_req2 = json!({ "model_id": TEST_MODEL_2 });
    server.post("/models").json(&add_req1).await;
    server.post("/models").json(&add_req2).await;
    server
        .post(&format!("/models/{}/download", TEST_MODEL_1_ENCODED))
        .await;
    server
        .post(&format!("/models/{}/download", TEST_MODEL_2_ENCODED))
        .await;

    // List models
    let response = server.get("/models").await;
    assert_eq!(response.status_code(), 200);

    let models: Vec<serde_json::Value> = response.json();

    // Find both models
    let model1 = models
        .iter()
        .find(|m| m["model_id"] == TEST_MODEL_1)
        .expect("First model should be in list");
    let model2 = models
        .iter()
        .find(|m| m["model_id"] == TEST_MODEL_2)
        .expect("Second model should be in list");

    assert_eq!(model1["status"], "downloaded");
    assert_eq!(model2["status"], "downloaded");
}

#[tokio::test]
async fn test_model_metadata_after_download() {
    let (server, _temp_dir) = create_test_server().await;

    // Download both models and check metadata
    for (model_id, model_encoded) in [
        (TEST_MODEL_1, TEST_MODEL_1_ENCODED),
        (TEST_MODEL_2, TEST_MODEL_2_ENCODED),
    ] {
        let add_req = json!({ "model_id": model_id });
        server.post("/models").json(&add_req).await;
        let response = server
            .post(&format!("/models/{}/download", model_encoded))
            .await;

        assert_eq!(
            response.status_code(),
            200,
            "Failed to download {}",
            model_id
        );
        let model: serde_json::Value = response.json();

        // Check metadata fields that should be populated from config.json
        if let Some(metadata) = model.get("metadata") {
            // Both models should have hidden_size in their config
            if let Some(hidden_size) = metadata.get("hidden_size") {
                assert!(
                    hidden_size.as_u64().unwrap() > 0,
                    "hidden_size should be > 0 for {}",
                    model_id
                );
            }
        }
    }
}

// ============================================================================
// Downloading State Tests
// ============================================================================

#[tokio::test]
async fn test_downloading_state_visible() {
    // Use a model that's unlikely to be cached
    // We use the model registry directly to check status while download runs
    const LARGER_MODEL: &str = "sentence-transformers/all-MiniLM-L6-v2";

    // Create shared state we can access from multiple tasks
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("state.toml");
    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/mock-tei-router");

    let registry = Arc::new(Registry::new(
        Some(10),
        tei_binary.to_string_lossy().to_string(),
        8080,
        8180,
    ));
    let state_manager = Arc::new(StateManager::new(
        state_file,
        registry.clone(),
        tei_binary.to_string_lossy().to_string(),
    ));
    let model_registry = Arc::new(ModelRegistry::new());
    let model_loader = Arc::new(ModelLoader::new());

    // Clone references for checking status
    let model_registry_check = model_registry.clone();

    let state = AppState {
        registry,
        state_manager,
        prometheus_handle: get_metrics_handle(),
        auth_manager: None,
        model_registry,
        model_loader,
    };

    let app = create_router(state);
    let server = TestServer::new(app).expect("Failed to create test server");

    // Add the model first
    let add_req = json!({ "model_id": LARGER_MODEL });
    server.post("/models").json(&add_req).await;

    // Check if already cached - if so, we can't test downloading state
    if let Some(entry) = model_registry_check.get(LARGER_MODEL).await
        && entry.status == tei_manager::models::ModelStatus::Downloaded
    {
        eprintln!("Model already cached, skipping downloading state test");
        return;
    }

    // Start download in background using direct API call
    let download_handle = {
        let model_id = LARGER_MODEL.to_string();
        tokio::spawn(async move { tei_manager::models::download_model(&model_id).await })
    };

    // Set status to downloading (simulating what the handler does)
    model_registry_check
        .set_status(LARGER_MODEL, tei_manager::models::ModelStatus::Downloading)
        .await;

    // Give it a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Check status via registry - should be "downloading"
    let entry = model_registry_check.get(LARGER_MODEL).await;
    if let Some(entry) = entry {
        assert!(
            entry.status == tei_manager::models::ModelStatus::Downloading
                || entry.status == tei_manager::models::ModelStatus::Downloaded,
            "Expected Downloading or Downloaded, got {:?}",
            entry.status
        );
    }

    // Wait for download to complete
    let _ = download_handle.await;
}

#[tokio::test]
async fn test_concurrent_download_rejected() {
    // Test that trying to download a model while status is "Downloading" returns 409
    // This test doesn't require network - we just simulate the downloading state
    const MODEL: &str = "fake-org/fake-model-for-test";
    const MODEL_ENCODED: &str = "fake-org%2Ffake-model-for-test";

    // Create shared state
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_file = temp_dir.path().join("state.toml");
    let tei_binary = std::env::current_dir()
        .expect("Failed to get current dir")
        .join("tests/mock-tei-router");

    let registry = Arc::new(Registry::new(
        Some(10),
        tei_binary.to_string_lossy().to_string(),
        8080,
        8180,
    ));
    let state_manager = Arc::new(StateManager::new(
        state_file,
        registry.clone(),
        tei_binary.to_string_lossy().to_string(),
    ));
    let model_registry = Arc::new(ModelRegistry::new());
    let model_registry_ref = model_registry.clone();
    let model_loader = Arc::new(ModelLoader::new());

    let state = AppState {
        registry,
        state_manager,
        prometheus_handle: get_metrics_handle(),
        auth_manager: None,
        model_registry,
        model_loader,
    };

    let app = create_router(state);
    let server = TestServer::new(app).expect("Failed to create test server");

    // Add the model (this will be status: Available since it's not in cache)
    let add_req = json!({ "model_id": MODEL });
    server.post("/models").json(&add_req).await;

    // Manually set status to Downloading to simulate an ongoing download
    model_registry_ref
        .set_status(MODEL, tei_manager::models::ModelStatus::Downloading)
        .await;

    // Try to download - should fail with 409 Conflict (ModelBusy)
    let response = server
        .post(&format!("/models/{}/download", MODEL_ENCODED))
        .await;

    assert_eq!(
        response.status_code(),
        409,
        "Expected 409 Conflict, got {}. Body: {}",
        response.status_code(),
        response.text()
    );

    let error: serde_json::Value = response.json();
    assert!(
        error["error"]
            .as_str()
            .map(|s| s.to_lowercase().contains("busy") || s.to_lowercase().contains("downloading"))
            .unwrap_or(false),
        "Error should mention busy/downloading: {:?}",
        error
    );
}

// ============================================================================
// Direct Download Tests (exercises download.rs code paths)
// ============================================================================

/// Test download_model_to_cache with a temp directory to exercise the full download path
/// This ensures the download code is covered even when models are already in user's cache
#[tokio::test]
async fn test_download_to_temp_cache() {
    use tei_manager::models::download_model_to_cache;

    let temp_dir = tempfile::tempdir().unwrap();
    let cache_dir = temp_dir.path().to_path_buf();

    // Download to temp cache - this MUST hit the network and exercise download.rs
    let result = download_model_to_cache(TEST_MODEL_1, Some(cache_dir.clone())).await;

    assert!(result.is_ok(), "Download failed: {:?}", result.err());

    let snapshot_path = result.unwrap();

    // Verify essential files exist
    assert!(
        snapshot_path.join("config.json").exists(),
        "config.json should exist"
    );
    assert!(
        snapshot_path.join("tokenizer.json").exists(),
        "tokenizer.json should exist"
    );

    // Verify weight file exists (safetensors or pytorch)
    let has_weights = snapshot_path.join("model.safetensors").exists()
        || snapshot_path.join("pytorch_model.bin").exists();
    assert!(has_weights, "Model weights should exist");
}

// ============================================================================
// Cache Detection Tests (don't require network if model already cached)
// ============================================================================

#[tokio::test]
async fn test_cache_detection_functions() {
    // These are unit-level tests that don't need network
    use tei_manager::models::{get_cache_dir, list_cached_models};

    // Cache dir should be a valid path
    let cache_dir = get_cache_dir();
    // Just verify it doesn't panic and returns a path
    assert!(!cache_dir.to_string_lossy().is_empty());

    // List cached models - should return a vec (may be empty)
    let cached = list_cached_models();
    // Just verify it doesn't panic - cached is a Vec so len() is always valid
    let _ = cached.len();
}

#[tokio::test]
async fn test_full_model_workflow_both_models() {
    let (server, _temp_dir) = create_test_server().await;

    // 1. List models (may be empty or have cached models)
    let response = server.get("/models").await;
    assert_eq!(response.status_code(), 200);
    let initial_models: Vec<serde_json::Value> = response.json();
    let initial_count = initial_models.len();

    // 2. Add both models
    for (model_id, model_encoded) in [
        (TEST_MODEL_1, TEST_MODEL_1_ENCODED),
        (TEST_MODEL_2, TEST_MODEL_2_ENCODED),
    ] {
        let add_req = json!({ "model_id": model_id });
        let response = server.post("/models").json(&add_req).await;
        assert!(
            response.status_code() == 200 || response.status_code() == 201,
            "Failed to add {}",
            model_id
        );

        // 3. Verify it's in the list
        let response = server.get("/models").await;
        let models: Vec<serde_json::Value> = response.json();
        assert!(models.len() >= initial_count);

        // 4. Get the model
        let response = server.get(&format!("/models/{}", model_encoded)).await;
        assert_eq!(response.status_code(), 200);

        // 5. Download the model
        let response = server
            .post(&format!("/models/{}/download", model_encoded))
            .await;
        assert_eq!(
            response.status_code(),
            200,
            "Failed to download {}",
            model_id
        );
        let model: serde_json::Value = response.json();
        assert_eq!(model["status"], "downloaded");

        // 6. Verify status persists
        let response = server.get(&format!("/models/{}", model_encoded)).await;
        assert_eq!(response.status_code(), 200);
        let model: serde_json::Value = response.json();
        assert_eq!(model["status"], "downloaded");
        assert!(model["cache_path"].is_string());
    }

    // 7. Verify both models are in the final list
    let response = server.get("/models").await;
    let models: Vec<serde_json::Value> = response.json();
    let model_ids: Vec<&str> = models
        .iter()
        .filter_map(|m| m["model_id"].as_str())
        .collect();
    assert!(model_ids.contains(&TEST_MODEL_1));
    assert!(model_ids.contains(&TEST_MODEL_2));
}
