//! API request handlers

use super::models::{CreateInstanceRequest, HealthResponse, InstanceInfo, LogsResponse};
use super::routes::AppState;
use crate::config::InstanceConfig;
use crate::error::TeiError;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;

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
) -> Result<Json<Vec<InstanceInfo>>, TeiError> {
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
) -> Result<(StatusCode, Json<InstanceInfo>), TeiError> {
    // Validate gpu_id if provided
    if let Some(gpu_id) = req.gpu_id {
        let gpu_info = crate::gpu::get_or_init();
        if !gpu_info.is_valid_gpu_id(gpu_id) {
            return Err(TeiError::InvalidGpuId {
                id: gpu_id,
                reason: format!("Available GPUs: {:?}", gpu_info.indices),
            });
        }
    }

    let config = InstanceConfig {
        name: req.name,
        model_id: req.model_id.clone(),
        port: req.port.unwrap_or(0), // 0 signals auto-allocation to registry
        max_batch_tokens: req.max_batch_tokens.unwrap_or(16384),
        max_concurrent_requests: req.max_concurrent_requests.unwrap_or(512),
        pooling: req.pooling,
        gpu_id: req.gpu_id,
        prometheus_port: req.prometheus_port,
        startup_timeout_secs: req.startup_timeout_secs,
        extra_args: req.extra_args.unwrap_or_default(),
        created_at: Some(chrono::Utc::now()),
    };

    let instance = state
        .registry
        .add(config)
        .await
        .map_err(|e| TeiError::ValidationError {
            message: e.to_string(),
        })?;

    instance
        .start(state.registry.tei_binary_path())
        .await
        .map_err(|e| TeiError::Internal {
            message: e.to_string(),
        })?;

    // Wait for instance to be ready (poll every 500ms, timeout after 5 minutes)
    // This runs in background so API returns immediately with "starting" status
    let instance_clone = instance.clone();
    tokio::spawn(async move {
        use crate::health::GrpcHealthChecker;
        use std::time::Duration;

        if let Err(e) = GrpcHealthChecker::wait_for_ready(
            &instance_clone,
            Duration::from_secs(300), // 5 minute timeout for model download
            Duration::from_millis(500),
        )
        .await
        {
            tracing::error!(
                instance = %instance_clone.config.name,
                error = %e,
                "Instance failed to become ready"
            );
            *instance_clone.status.write().await = crate::instance::InstanceStatus::Failed;
        }
    });

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
) -> Result<Json<InstanceInfo>, TeiError> {
    let instance = state
        .registry
        .get(&name)
        .await
        .ok_or_else(|| TeiError::InstanceNotFound { name: name.clone() })?;

    let info = InstanceInfo::from_instance(&instance).await;

    Ok(Json(info))
}

/// DELETE /instances/:name - Delete instance
pub async fn delete_instance(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, TeiError> {
    state
        .registry
        .remove(&name)
        .await
        .map_err(|_| TeiError::InstanceNotFound { name: name.clone() })?;

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
) -> Result<Json<InstanceInfo>, TeiError> {
    let instance = state
        .registry
        .get(&name)
        .await
        .ok_or_else(|| TeiError::InstanceNotFound { name: name.clone() })?;

    instance
        .start(state.registry.tei_binary_path())
        .await
        .map_err(|e| TeiError::Internal {
            message: e.to_string(),
        })?;

    // Wait for instance to be ready in background
    let instance_clone = instance.clone();
    tokio::spawn(async move {
        use crate::health::GrpcHealthChecker;
        use std::time::Duration;

        if let Err(e) = GrpcHealthChecker::wait_for_ready(
            &instance_clone,
            Duration::from_secs(300),
            Duration::from_millis(500),
        )
        .await
        {
            tracing::error!(
                instance = %instance_clone.config.name,
                error = %e,
                "Instance failed to become ready"
            );
            *instance_clone.status.write().await = crate::instance::InstanceStatus::Failed;
        }
    });

    let info = InstanceInfo::from_instance(&instance).await;

    Ok(Json(info))
}

/// POST /instances/:name/stop - Stop a running instance
pub async fn stop_instance(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<InstanceInfo>, TeiError> {
    let instance = state
        .registry
        .get(&name)
        .await
        .ok_or_else(|| TeiError::InstanceNotFound { name: name.clone() })?;

    instance.stop().await.map_err(|e| TeiError::Internal {
        message: e.to_string(),
    })?;

    let info = InstanceInfo::from_instance(&instance).await;

    Ok(Json(info))
}

/// POST /instances/:name/restart - Restart an instance
pub async fn restart_instance(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<InstanceInfo>, TeiError> {
    let instance = state
        .registry
        .get(&name)
        .await
        .ok_or_else(|| TeiError::InstanceNotFound { name: name.clone() })?;

    instance
        .restart(state.registry.tei_binary_path())
        .await
        .map_err(|e| TeiError::Internal {
            message: e.to_string(),
        })?;

    let info = InstanceInfo::from_instance(&instance).await;

    Ok(Json(info))
}

/// Query parameters for log slicing
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub start: Option<i32>,
    pub end: Option<i32>,
}

/// GET /instances/{name}/logs - Get instance logs with Python-style slicing
pub async fn get_logs(
    Path(name): Path<String>,
    Query(params): Query<LogsQuery>,
) -> Result<Json<LogsResponse>, TeiError> {
    // Use same log directory resolution as spawn
    let log_dir_path =
        std::env::var("TEI_MANAGER_LOG_DIR").unwrap_or_else(|_| "/data/logs".to_string());

    let log_dir = std::path::Path::new(&log_dir_path);

    // Check fallback location if primary doesn't exist
    let log_path = if !log_dir.exists() {
        std::path::Path::new("/tmp/tei-manager/logs").join(format!("{}.log", name))
    } else {
        log_dir.join(format!("{}.log", name))
    };

    if !log_path.exists() {
        return Err(TeiError::InstanceNotFound { name });
    }

    let content = tokio::fs::read_to_string(&log_path)
        .await
        .map_err(|e| TeiError::IoError {
            message: format!("Failed to read log file: {}", e),
        })?;

    // Count lines first without allocating
    let total_lines = content.lines().count();

    // Python-style slicing [start, end) with negative index support
    let start_idx = params
        .start
        .map(|s| {
            if s < 0 {
                (total_lines as i32 + s).max(0) as usize
            } else {
                (s as usize).min(total_lines)
            }
        })
        .unwrap_or(0);

    let end_idx = params
        .end
        .map(|e| {
            if e < 0 {
                (total_lines as i32 + e).max(0) as usize
            } else {
                (e as usize).min(total_lines)
            }
        })
        .unwrap_or(total_lines);

    // Only allocate strings for the requested slice
    let lines: Vec<String> = if start_idx < end_idx {
        content
            .lines()
            .skip(start_idx)
            .take(end_idx - start_idx)
            .map(String::from)
            .collect()
    } else {
        Vec::new()
    };

    Ok(Json(LogsResponse {
        lines,
        start: start_idx,
        end: end_idx,
        total_lines,
    }))
}
