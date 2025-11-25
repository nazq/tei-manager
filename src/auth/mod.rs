//! Authentication and authorization module

use async_trait::async_trait;
use axum::http::HeaderMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tonic::metadata::MetadataMap;

pub mod mtls;
pub mod service;

pub use mtls::MtlsProvider;
pub use service::AuthService;

/// Authentication errors
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Missing client certificate")]
    MissingClientCert,

    #[error("Invalid client certificate: {0}")]
    InvalidCert(String),

    #[error("Certificate verification failed: {0}")]
    CertVerificationFailed(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Authentication provider error: {0}")]
    ProviderError(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

/// Protocol type for the request
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Grpc,
}

/// TLS connection information
#[derive(Debug, Clone)]
pub struct TlsInfo {
    /// Peer certificate (DER encoded)
    pub peer_certificate: Option<Vec<u8>>,

    /// Certificate chain (DER encoded)
    pub certificate_chain: Vec<Vec<u8>>,

    /// TLS version
    pub tls_version: String,

    /// Cipher suite
    pub cipher_suite: String,
}

/// Authentication request context
#[derive(Debug, Clone)]
pub struct AuthRequest {
    /// Protocol being used
    pub protocol: Protocol,

    /// Peer address
    pub peer_addr: SocketAddr,

    /// HTTP headers (for HTTP requests)
    pub headers: Option<HeaderMap>,

    /// gRPC metadata (for gRPC requests)
    pub metadata: Option<MetadataMap>,

    /// TLS connection information
    pub tls_info: Option<TlsInfo>,
}

/// Authentication result
#[derive(Debug, Clone)]
pub struct AuthResult {
    /// Whether authentication succeeded
    pub authenticated: bool,

    /// Principal/identity of the authenticated entity
    pub principal: Option<String>,

    /// Additional metadata about the authentication
    pub metadata: HashMap<String, String>,
}

/// Authentication provider trait
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Authenticate a request
    async fn authenticate(&self, request: &AuthRequest) -> Result<AuthResult, AuthError>;

    /// Whether this provider supports HTTP
    fn supports_http(&self) -> bool;

    /// Whether this provider supports gRPC
    fn supports_grpc(&self) -> bool;

    /// Provider name for logging/metrics
    fn name(&self) -> &str;
}

/// Multi-provider authentication service
pub struct AuthManager {
    providers: Vec<Arc<dyn AuthProvider>>,
}

impl AuthManager {
    /// Create a new AuthManager with the given providers
    pub fn new(providers: Vec<Arc<dyn AuthProvider>>) -> Self {
        Self { providers }
    }

    /// Authenticate a request using all configured providers
    /// Returns success if ANY provider succeeds (OR logic)
    pub async fn authenticate(&self, request: &AuthRequest) -> Result<AuthResult, AuthError> {
        if self.providers.is_empty() {
            return Err(AuthError::Internal(
                "No authentication providers configured".to_string(),
            ));
        }

        let mut last_error = None;

        for provider in &self.providers {
            // Skip providers that don't support this protocol
            match request.protocol {
                Protocol::Http if !provider.supports_http() => continue,
                Protocol::Grpc if !provider.supports_grpc() => continue,
                _ => {}
            }

            match provider.authenticate(request).await {
                Ok(result) if result.authenticated => {
                    tracing::info!(
                        provider = provider.name(),
                        principal = ?result.principal,
                        "Authentication successful"
                    );
                    return Ok(result);
                }
                Ok(_) => {
                    tracing::debug!(
                        provider = provider.name(),
                        "Authentication failed: not authenticated"
                    );
                    last_error = Some(AuthError::Unauthorized(format!(
                        "{} authentication failed",
                        provider.name()
                    )));
                }
                Err(e) => {
                    tracing::debug!(
                        provider = provider.name(),
                        error = %e,
                        "Authentication error"
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            AuthError::Unauthorized("No compatible authentication provider found".to_string())
        }))
    }

    /// Check if auth manager is empty
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Number of configured providers
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestProvider {
        name: String,
        should_succeed: bool,
        supports_http: bool,
        supports_grpc: bool,
    }

    #[async_trait]
    impl AuthProvider for TestProvider {
        async fn authenticate(&self, _request: &AuthRequest) -> Result<AuthResult, AuthError> {
            if self.should_succeed {
                Ok(AuthResult {
                    authenticated: true,
                    principal: Some("test-user".to_string()),
                    metadata: HashMap::new(),
                })
            } else {
                Err(AuthError::Unauthorized("test failure".to_string()))
            }
        }

        fn supports_http(&self) -> bool {
            self.supports_http
        }

        fn supports_grpc(&self) -> bool {
            self.supports_grpc
        }

        fn name(&self) -> &str {
            &self.name
        }
    }

    #[tokio::test]
    async fn test_auth_manager_success() {
        let provider = Arc::new(TestProvider {
            name: "test".to_string(),
            should_succeed: true,
            supports_http: true,
            supports_grpc: true,
        });

        let manager = AuthManager::new(vec![provider]);

        let request = AuthRequest {
            protocol: Protocol::Http,
            peer_addr: "127.0.0.1:1234".parse().unwrap(),
            headers: None,
            metadata: None,
            tls_info: None,
        };

        let result = manager.authenticate(&request).await.unwrap();
        assert!(result.authenticated);
        assert_eq!(result.principal, Some("test-user".to_string()));
    }

    #[tokio::test]
    async fn test_auth_manager_failure() {
        let provider = Arc::new(TestProvider {
            name: "test".to_string(),
            should_succeed: false,
            supports_http: true,
            supports_grpc: true,
        });

        let manager = AuthManager::new(vec![provider]);

        let request = AuthRequest {
            protocol: Protocol::Http,
            peer_addr: "127.0.0.1:1234".parse().unwrap(),
            headers: None,
            metadata: None,
            tls_info: None,
        };

        let result = manager.authenticate(&request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_auth_manager_multiple_providers_fallback() {
        let failing_provider = Arc::new(TestProvider {
            name: "failing".to_string(),
            should_succeed: false,
            supports_http: true,
            supports_grpc: true,
        });

        let succeeding_provider = Arc::new(TestProvider {
            name: "succeeding".to_string(),
            should_succeed: true,
            supports_http: true,
            supports_grpc: true,
        });

        let manager = AuthManager::new(vec![failing_provider, succeeding_provider]);

        let request = AuthRequest {
            protocol: Protocol::Http,
            peer_addr: "127.0.0.1:1234".parse().unwrap(),
            headers: None,
            metadata: None,
            tls_info: None,
        };

        // Should succeed because second provider succeeds
        let result = manager.authenticate(&request).await.unwrap();
        assert!(result.authenticated);
    }

    #[tokio::test]
    async fn test_auth_manager_protocol_filtering() {
        let http_only_provider = Arc::new(TestProvider {
            name: "http-only".to_string(),
            should_succeed: true,
            supports_http: true,
            supports_grpc: false,
        });

        let manager = AuthManager::new(vec![http_only_provider]);

        let grpc_request = AuthRequest {
            protocol: Protocol::Grpc,
            peer_addr: "127.0.0.1:1234".parse().unwrap(),
            headers: None,
            metadata: None,
            tls_info: None,
        };

        // Should fail because provider doesn't support gRPC
        let result = manager.authenticate(&grpc_request).await;
        assert!(result.is_err());
    }
}
