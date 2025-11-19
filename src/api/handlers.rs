//! API request handlers

use super::models::{CreateInstanceRequest, HealthResponse, InstanceInfo};
use super::routes::AppState;
use crate::config::InstanceConfig;
use crate::error::ApiError;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};

/// GET /health - Manager health check
pub async fn health() -> (StatusCode, Json<HealthResponse>) {
    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "healthy".to_string(),
            timestamp: chrono::Utc::now(),
        }),
    )
}

/// GET /metrics - Prometheus metrics
pub async fn metrics(State(state): State<AppState>) -> String {
    state.prometheus_handle.render()
}

/// GET /instances - List all instances
pub async fn list_instances(
    State(state): State<AppState>,
) -> Result<Json<Vec<InstanceInfo>>, ApiError> {
    let instances = state.registry.list().await;

    let mut info_list = Vec::new();
    for instance in instances {
        info_list.push(InstanceInfo::from_instance(&instance).await);
    }

    // Update metrics
    crate::metrics::update_instance_count(info_list.len());

    Ok(Json(info_list))
}

/// POST /instances - Create and start a new instance
pub async fn create_instance(
    State(state): State<AppState>,
    Json(req): Json<CreateInstanceRequest>,
) -> Result<(StatusCode, Json<InstanceInfo>), ApiError> {
    let config = InstanceConfig {
        name: req.name,
        model_id: req.model_id.clone(),
        port: req.port,
        max_batch_tokens: req.max_batch_tokens.unwrap_or(16384),
        max_concurrent_requests: req.max_concurrent_requests.unwrap_or(512),
        pooling: req.pooling,
        gpu_id: req.gpu_id,
        prometheus_port: req.prometheus_port,
        extra_args: req.extra_args.unwrap_or_default(),
        created_at: Some(chrono::Utc::now()),
    };

    let instance = state
        .registry
        .add(config)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    instance
        .start(state.registry.tei_binary_path())
        .await
        .map_err(ApiError::Internal)?;

    // Save state asynchronously
    let state_manager = state.state_manager.clone();
    tokio::spawn(async move {
        if let Err(e) = state_manager.save().await {
            tracing::error!(error = %e, "Failed to save state");
        }
    });

    // Record metrics
    crate::metrics::record_instance_created(&instance.config.name, &req.model_id);
    crate::metrics::update_instance_count(state.registry.count().await);

    let info = InstanceInfo::from_instance(&instance).await;

    Ok((StatusCode::CREATED, Json(info)))
}

/// GET /instances/:name - Get instance details
pub async fn get_instance(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<InstanceInfo>, ApiError> {
    let instance = state
        .registry
        .get(&name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Instance '{}' not found", name)))?;

    let info = InstanceInfo::from_instance(&instance).await;

    Ok(Json(info))
}

/// DELETE /instances/:name - Delete instance
pub async fn delete_instance(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .registry
        .remove(&name)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;

    // Save state asynchronously
    let state_manager = state.state_manager.clone();
    tokio::spawn(async move {
        let _ = state_manager.save().await;
    });

    // Record metrics
    crate::metrics::record_instance_deleted(&name);
    crate::metrics::update_instance_count(state.registry.count().await);

    Ok(StatusCode::NO_CONTENT)
}

/// POST /instances/:name/start - Start a stopped instance
pub async fn start_instance(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<InstanceInfo>, ApiError> {
    let instance = state
        .registry
        .get(&name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Instance '{}' not found", name)))?;

    instance
        .start(state.registry.tei_binary_path())
        .await
        .map_err(ApiError::Internal)?;

    let info = InstanceInfo::from_instance(&instance).await;

    Ok(Json(info))
}

/// POST /instances/:name/stop - Stop a running instance
pub async fn stop_instance(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<InstanceInfo>, ApiError> {
    let instance = state
        .registry
        .get(&name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Instance '{}' not found", name)))?;

    instance.stop().await.map_err(ApiError::Internal)?;

    let info = InstanceInfo::from_instance(&instance).await;

    Ok(Json(info))
}

/// POST /instances/:name/restart - Restart an instance
pub async fn restart_instance(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<InstanceInfo>, ApiError> {
    let instance = state
        .registry
        .get(&name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Instance '{}' not found", name)))?;

    instance
        .restart(state.registry.tei_binary_path())
        .await
        .map_err(ApiError::Internal)?;

    let info = InstanceInfo::from_instance(&instance).await;

    Ok(Json(info))
}
