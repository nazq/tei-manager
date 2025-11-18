//! API route definitions

use crate::registry::Registry;
use crate::state::StateManager;
use axum::{
    Router,
    routing::{delete, get, post},
};
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use super::handlers;

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    pub state_manager: Arc<StateManager>,
    pub prometheus_handle: metrics_exporter_prometheus::PrometheusHandle,
}

/// Create the main API router
pub fn create_router(state: AppState) -> Router {
    Router::new()
        // Health and status
        .route("/health", get(handlers::health))
        .route("/metrics", get(handlers::metrics))
        // Instance management (no PATCH - delete and recreate instead)
        .route("/instances", get(handlers::list_instances))
        .route("/instances", post(handlers::create_instance))
        .route("/instances/:name", get(handlers::get_instance))
        .route("/instances/:name", delete(handlers::delete_instance))
        // Instance lifecycle
        .route("/instances/:name/start", post(handlers::start_instance))
        .route("/instances/:name/stop", post(handlers::stop_instance))
        .route("/instances/:name/restart", post(handlers::restart_instance))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::permissive()),
        )
}
