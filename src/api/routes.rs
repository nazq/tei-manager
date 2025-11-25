//! API route definitions

use crate::auth::AuthManager;
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
    pub auth_manager: Option<Arc<AuthManager>>,
}

/// Create the main API router
pub fn create_router(state: AppState) -> Router {
    let auth_manager = state.auth_manager.clone();

    let mut router = Router::new()
        // Health and status (always public)
        .route("/health", get(handlers::health))
        .route("/metrics", get(handlers::metrics));

    // Protected routes - require auth if enabled
    let protected_routes = Router::new()
        // Instance management (no PATCH - delete and recreate instead)
        .route("/instances", get(handlers::list_instances))
        .route("/instances", post(handlers::create_instance))
        .route("/instances/{name}", get(handlers::get_instance))
        .route("/instances/{name}", delete(handlers::delete_instance))
        // Instance lifecycle
        .route("/instances/{name}/start", post(handlers::start_instance))
        .route("/instances/{name}/stop", post(handlers::stop_instance))
        .route(
            "/instances/{name}/restart",
            post(handlers::restart_instance),
        )
        // Instance logs
        .route("/instances/{name}/logs", get(handlers::get_logs));

    // Add auth middleware to protected routes if auth is enabled
    let protected_routes = if let Some(auth) = auth_manager {
        tracing::info!("Auth enabled - protecting instance management endpoints");
        protected_routes.layer(axum::middleware::from_fn(move |req, next| {
            let auth = auth.clone();
            async move { crate::auth::service::auth_middleware(auth, req, next).await }
        }))
    } else {
        tracing::warn!("Auth disabled - instance management endpoints are PUBLIC");
        protected_routes
    };

    router = router.merge(protected_routes);

    router.with_state(state).layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .layer(CorsLayer::permissive()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;
    use crate::registry::Registry;
    use crate::state::StateManager;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
    use std::sync::OnceLock;
    use tower::ServiceExt;

    // Global prometheus handle to avoid multiple recorder installations
    static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

    fn get_prometheus_handle() -> PrometheusHandle {
        PROMETHEUS_HANDLE
            .get_or_init(|| {
                PrometheusBuilder::new()
                    .install_recorder()
                    .expect("Prometheus recorder should install")
            })
            .clone()
    }

    fn create_test_state() -> AppState {
        let registry = Arc::new(Registry::new(
            Some(10),
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let state_manager = Arc::new(StateManager::new(
            "/tmp/test-state.toml".into(),
            registry.clone(),
            "text-embeddings-router".to_string(),
        ));
        let prometheus_handle = get_prometheus_handle();

        AppState {
            registry,
            state_manager,
            prometheus_handle,
            auth_manager: None,
        }
    }

    fn create_test_state_with_auth() -> AppState {
        let mut state = create_test_state();
        // Create a minimal auth manager for testing
        let providers: Vec<Arc<dyn crate::auth::AuthProvider>> = vec![];
        state.auth_manager = Some(Arc::new(AuthManager::new(providers)));
        state
    }

    #[tokio::test]
    async fn test_create_router_without_auth() {
        let state = create_test_state();
        let app = create_router(state);

        // Test health endpoint
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_create_router_with_auth() {
        let state = create_test_state_with_auth();
        let app = create_router(state);

        // Test health endpoint (should be public even with auth)
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        let state = create_test_state();
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_instances_endpoint() {
        let state = create_test_state();
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/instances")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_nonexistent_instance() {
        let state = create_test_state();
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/instances/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_app_state_clone() {
        let state = create_test_state();
        let cloned = state.clone();

        // Both should have same registry reference
        assert!(Arc::ptr_eq(&state.registry, &cloned.registry));
        assert!(Arc::ptr_eq(&state.state_manager, &cloned.state_manager));
    }

    #[tokio::test]
    async fn test_protected_routes_without_auth() {
        let state = create_test_state();
        let app = create_router(state);

        // Without auth enabled, protected routes should work
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/instances")
                    .method("GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should succeed without auth
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_protected_routes_with_empty_auth() {
        let state = create_test_state_with_auth();
        let app = create_router(state);

        // With auth manager but empty providers, should pass through
        // (native TLS mode where rustls already verified)
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/instances")
                    .method("GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // With empty providers and no cert header, defaults to passing (native TLS assumption)
        assert_eq!(response.status(), StatusCode::OK);
    }
}
