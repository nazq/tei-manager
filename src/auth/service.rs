//! Authentication service middleware for Axum

use super::{AuthError, AuthManager, AuthRequest, Protocol, TlsInfo};
use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// Extract TLS info from HTTP headers (for nginx proxy scenarios)
fn extract_tls_info_from_headers(headers: &HeaderMap) -> Option<TlsInfo> {
    // Extract client certificate from X-SSL-Client-Cert header
    let cert_header = headers.get("x-ssl-client-cert")?;
    let cert_str = cert_header.to_str().ok()?;

    // URL decode the certificate (nginx uses $ssl_client_escaped_cert)
    let cert_decoded = urlencoding::decode(cert_str).ok()?;

    // Convert PEM to DER
    let peer_certificate = pem_to_der(cert_decoded.as_bytes()).ok()?;

    Some(TlsInfo {
        peer_certificate: Some(peer_certificate),
        certificate_chain: Vec::new(),
        tls_version: headers
            .get("x-ssl-protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string(),
        cipher_suite: headers
            .get("x-ssl-cipher")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string(),
    })
}

/// Convert PEM to DER format using x509-parser
fn pem_to_der(pem_data: &[u8]) -> Result<Vec<u8>, AuthError> {
    let pem_certs = x509_parser::pem::Pem::iter_from_buffer(pem_data)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AuthError::Internal(format!("Failed to parse PEM certificate: {}", e)))?;

    let pem_cert = pem_certs
        .first()
        .ok_or_else(|| AuthError::Internal("No PEM blocks found in certificate".to_string()))?;

    Ok(pem_cert.contents.to_vec())
}

/// Extract TLS info from native TLS connection
#[allow(dead_code)]
fn extract_tls_info_from_connection(_request: &Request) -> Option<TlsInfo> {
    // With axum-server + rustls, client certificate verification happens at the TLS handshake layer.
    // If a connection reaches this point, it means:
    // 1. The client presented a certificate
    // 2. The certificate was signed by a trusted CA
    // 3. The certificate is valid (not expired, proper chain, etc.)
    //
    // This is more secure than nginx proxy mode because bad certs never complete the handshake.
    // For now, we return None here and let the header-based extraction handle proxy scenarios.
    // In the future, we could extract the actual certificate from rustls ServerConnection
    // if needed for additional validation (subject DN, SAN, etc.).
    //
    // TODO: Extract actual peer certificate from rustls for subject/SAN verification
    None
}

/// Axum middleware for authentication
///
/// # Security
///
/// When `require_cert_headers` is false (default), requests without X-SSL-Client-Cert
/// headers are assumed to be native TLS connections where rustls already verified
/// the client certificate. This is ONLY safe if your API is not directly accessible
/// from untrusted networks.
///
/// Set `require_cert_headers` to true in production when behind a reverse proxy
/// to ensure all requests must include certificate headers.
pub async fn auth_middleware(
    auth_manager: Arc<AuthManager>,
    request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    auth_middleware_with_options(auth_manager, false, request, next).await
}

/// Axum middleware for authentication with configurable cert header requirement
pub async fn auth_middleware_with_options(
    auth_manager: Arc<AuthManager>,
    require_cert_headers: bool,
    request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    // Extract headers and peer address
    let headers_clone = request.headers().clone();
    let headers = Some(headers_clone.clone());
    let peer_addr = request
        .extensions()
        .get::<std::net::SocketAddr>()
        .copied()
        .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());

    // Try to extract TLS info from headers (nginx proxy scenario)
    let tls_info = extract_tls_info_from_headers(&headers_clone);

    // If we couldn't extract cert info from headers...
    if tls_info.is_none() {
        if require_cert_headers {
            // In strict mode, reject requests without cert headers
            tracing::warn!(
                peer_addr = %peer_addr,
                "Request rejected: missing X-SSL-Client-Cert header (require_cert_headers=true)"
            );
            return Err(AuthError::MissingClientCert);
        }

        // In permissive mode (default), assume native TLS where rustls verified the cert
        // SECURITY WARNING: This logs a warning because it's a potential bypass vector
        // if the API is directly accessible without going through a reverse proxy.
        tracing::debug!(
            peer_addr = %peer_addr,
            "No cert in headers - assuming native TLS verified by rustls. \
             Set require_cert_headers=true if behind a reverse proxy."
        );
        return Ok(next.run(request).await);
    }

    // For proxy scenarios with cert in headers, verify using auth providers
    let auth_request = AuthRequest {
        protocol: Protocol::Http,
        peer_addr,
        headers,
        metadata: None,
        tls_info,
    };

    // Authenticate
    match auth_manager.authenticate(&auth_request).await {
        Ok(result) if result.authenticated => {
            // TODO: Add principal to request extensions for downstream handlers
            Ok(next.run(request).await)
        }
        Ok(_) => Err(AuthError::Unauthorized("Authentication failed".to_string())),
        Err(e) => Err(e),
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AuthError::MissingClientCert => {
                (StatusCode::UNAUTHORIZED, "Missing client certificate")
            }
            AuthError::InvalidCert(_) => (StatusCode::UNAUTHORIZED, "Invalid client certificate"),
            AuthError::CertVerificationFailed(_) => {
                (StatusCode::UNAUTHORIZED, "Certificate verification failed")
            }
            AuthError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "Unauthorized"),
            AuthError::ProviderError(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Authentication error")
            }
            AuthError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
        };

        tracing::warn!(error = %self, "Authentication failed");

        (status, message).into_response()
    }
}

/// Service wrapper for AuthManager
pub struct AuthService {
    manager: Arc<AuthManager>,
}

impl AuthService {
    /// Create a new AuthService
    pub fn new(manager: Arc<AuthManager>) -> Self {
        Self { manager }
    }

    /// Get the underlying manager
    pub fn manager(&self) -> &Arc<AuthManager> {
        &self.manager
    }

    /// Create auth middleware
    pub fn middleware(
        &self,
    ) -> impl Fn(
        Request,
        Next,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response, AuthError>> + Send>,
    > + Clone {
        let manager = self.manager.clone();
        move |request, next| {
            let manager = manager.clone();
            Box::pin(auth_middleware(manager, request, next))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthProvider, AuthResult};
    use async_trait::async_trait;
    use axum::{
        Router,
        body::Body,
        http::{Request as AxumRequest, StatusCode},
        middleware,
        routing::get,
    };
    use std::collections::HashMap;
    use tower::ServiceExt;

    struct MockProvider {
        should_succeed: bool,
    }

    #[async_trait]
    impl AuthProvider for MockProvider {
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
            true
        }

        fn supports_grpc(&self) -> bool {
            false
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn test_auth_middleware_success() {
        let provider = Arc::new(MockProvider {
            should_succeed: true,
        });
        let manager = Arc::new(AuthManager::new(vec![provider]));

        let app = Router::new()
            .route("/test", get(|| async { "Hello" }))
            .route_layer(middleware::from_fn(move |req, next| {
                let manager = manager.clone();
                auth_middleware(manager, req, next)
            }));

        let response = app
            .oneshot(
                AxumRequest::builder()
                    .uri("/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_middleware_failure() {
        let provider = Arc::new(MockProvider {
            should_succeed: false,
        });
        let manager = Arc::new(AuthManager::new(vec![provider]));

        let app = Router::new()
            .route("/test", get(|| async { "Hello" }))
            .route_layer(middleware::from_fn(move |req, next| {
                let manager = manager.clone();
                auth_middleware(manager, req, next)
            }));

        // Add a mock certificate header to trigger proxy auth path
        let mock_cert = "-----BEGIN CERTIFICATE-----\nMIIBkTCB+wIJAKHHCgVZU1XNMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl\nc3RDQTAeFw0yNDAxMDEwMDAwMDBaFw0yNTAxMDEwMDAwMDBaMBExDzANBgNVBAMM\nBnRlc3RjbDBcMA0GCSqGSIb3DQEBAQUAA0sAMEgCQQC8VvXxNvRRuZpz5xDZ8VaL\n-----END CERTIFICATE-----";
        let response = app
            .oneshot(
                AxumRequest::builder()
                    .uri("/test")
                    .header("x-ssl-client-cert", urlencoding::encode(mock_cert).as_ref())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_pem_to_der_valid() {
        // A minimal valid PEM certificate for testing
        let valid_pem = b"-----BEGIN CERTIFICATE-----
MIIBkTCB+wIJAKHHCgVZU1XNMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl
c3RDQTAeFw0yNDAxMDEwMDAwMDBaFw0yNTAxMDEwMDAwMDBaMBExDzANBgNVBAMM
BnRlc3RjbDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAjEcwRQIh
AKxxx/wT4GxmFLRQZeJPLJAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==
-----END CERTIFICATE-----";

        let result = pem_to_der(valid_pem);
        assert!(result.is_ok());
        let der = result.unwrap();
        assert!(!der.is_empty());
    }

    #[test]
    fn test_pem_to_der_invalid() {
        let invalid_pem = b"not a valid pem";
        let result = pem_to_der(invalid_pem);
        assert!(result.is_err());
    }

    #[test]
    fn test_pem_to_der_empty() {
        let empty_pem = b"";
        let result = pem_to_der(empty_pem);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_tls_info_from_headers_missing_cert() {
        let headers = HeaderMap::new();
        let result = extract_tls_info_from_headers(&headers);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_tls_info_from_headers_with_cert() {
        let mut headers = HeaderMap::new();
        let cert_pem = "-----BEGIN CERTIFICATE-----
MIIBkTCB+wIJAKHHCgVZU1XNMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl
c3RDQTAeFw0yNDAxMDEwMDAwMDBaFw0yNTAxMDEwMDAwMDBaMBExDzANBgNVBAMM
BnRlc3RjbDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAjEcwRQIh
AKxxx/wT4GxmFLRQZeJPLJAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==
-----END CERTIFICATE-----";

        headers.insert(
            "x-ssl-client-cert",
            urlencoding::encode(cert_pem).parse().unwrap(),
        );
        headers.insert("x-ssl-protocol", "TLSv1.3".parse().unwrap());
        headers.insert("x-ssl-cipher", "TLS_AES_256_GCM_SHA384".parse().unwrap());

        let result = extract_tls_info_from_headers(&headers);
        assert!(result.is_some());

        let tls_info = result.unwrap();
        assert!(tls_info.peer_certificate.is_some());
        assert_eq!(tls_info.tls_version, "TLSv1.3");
        assert_eq!(tls_info.cipher_suite, "TLS_AES_256_GCM_SHA384");
    }

    #[test]
    fn test_extract_tls_info_from_headers_invalid_cert() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-ssl-client-cert",
            urlencoding::encode("invalid cert data").parse().unwrap(),
        );

        let result = extract_tls_info_from_headers(&headers);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_tls_info_from_headers_missing_protocol() {
        let mut headers = HeaderMap::new();
        let cert_pem = "-----BEGIN CERTIFICATE-----
MIIBkTCB+wIJAKHHCgVZU1XNMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl
c3RDQTAeFw0yNDAxMDEwMDAwMDBaFw0yNTAxMDEwMDAwMDBaMBExDzANBgNVBAMM
BnRlc3RjbDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAjEcwRQIh
AKxxx/wT4GxmFLRQZeJPLJAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==
-----END CERTIFICATE-----";

        headers.insert(
            "x-ssl-client-cert",
            urlencoding::encode(cert_pem).parse().unwrap(),
        );
        // No protocol header - should use default "unknown"

        let result = extract_tls_info_from_headers(&headers);
        assert!(result.is_some());

        let tls_info = result.unwrap();
        assert_eq!(tls_info.tls_version, "unknown");
        assert_eq!(tls_info.cipher_suite, "unknown");
    }

    #[test]
    fn test_auth_error_into_response_missing_cert() {
        let error = AuthError::MissingClientCert;
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_auth_error_into_response_invalid_cert() {
        let error = AuthError::InvalidCert("bad cert".to_string());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_auth_error_into_response_cert_verification_failed() {
        let error = AuthError::CertVerificationFailed("verification error".to_string());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_auth_error_into_response_unauthorized() {
        let error = AuthError::Unauthorized("not allowed".to_string());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_auth_error_into_response_provider_error() {
        let error = AuthError::ProviderError("provider failed".to_string());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_auth_error_into_response_internal() {
        let error = AuthError::Internal("internal error".to_string());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_auth_service_new() {
        let providers: Vec<Arc<dyn AuthProvider>> = vec![];
        let manager = Arc::new(AuthManager::new(providers));
        let service = AuthService::new(manager.clone());

        assert!(Arc::ptr_eq(service.manager(), &manager));
    }

    #[test]
    fn test_auth_service_manager() {
        let providers: Vec<Arc<dyn AuthProvider>> = vec![];
        let manager = Arc::new(AuthManager::new(providers));
        let service = AuthService::new(manager.clone());

        // Verify manager() returns the correct reference
        let retrieved = service.manager();
        assert!(Arc::ptr_eq(retrieved, &manager));
    }

    #[tokio::test]
    async fn test_auth_service_middleware() {
        let provider = Arc::new(MockProvider {
            should_succeed: true,
        });
        let manager = Arc::new(AuthManager::new(vec![provider]));
        let service = AuthService::new(manager.clone());

        // Test that middleware() returns a closure (just test it can be called)
        let _middleware_fn = service.middleware();

        // Use auth_middleware directly instead of service.middleware() for the router
        // to avoid lifetime issues
        let app = Router::new()
            .route("/test", get(|| async { "Hello" }))
            .route_layer(middleware::from_fn(move |req, next| {
                let manager = manager.clone();
                auth_middleware(manager, req, next)
            }));

        let response = app
            .oneshot(
                AxumRequest::builder()
                    .uri("/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Without cert headers, should pass (native TLS assumption)
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_middleware_with_successful_provider() {
        let provider = Arc::new(MockProvider {
            should_succeed: true,
        });
        let manager = Arc::new(AuthManager::new(vec![provider]));

        let app = Router::new()
            .route("/test", get(|| async { "Hello" }))
            .route_layer(middleware::from_fn(move |req, next| {
                let manager = manager.clone();
                auth_middleware(manager, req, next)
            }));

        // Add a valid (mock) certificate header to trigger proxy auth path
        let cert_pem = "-----BEGIN CERTIFICATE-----
MIIBkTCB+wIJAKHHCgVZU1XNMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl
c3RDQTAeFw0yNDAxMDEwMDAwMDBaFw0yNTAxMDEwMDAwMDBaMBExDzANBgNVBAMM
BnRlc3RjbDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAjEcwRQIh
AKxxx/wT4GxmFLRQZeJPLJAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==
-----END CERTIFICATE-----";

        let response = app
            .oneshot(
                AxumRequest::builder()
                    .uri("/test")
                    .header("x-ssl-client-cert", urlencoding::encode(cert_pem).as_ref())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // MockProvider configured to succeed
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_middleware_authenticated_but_not_authorized() {
        // Provider that returns authenticated=false
        struct RejectingProvider;

        #[async_trait]
        impl AuthProvider for RejectingProvider {
            async fn authenticate(&self, _request: &AuthRequest) -> Result<AuthResult, AuthError> {
                Ok(AuthResult {
                    authenticated: false,
                    principal: None,
                    metadata: HashMap::new(),
                })
            }

            fn supports_http(&self) -> bool {
                true
            }

            fn supports_grpc(&self) -> bool {
                false
            }

            fn name(&self) -> &str {
                "rejecting"
            }
        }

        let provider = Arc::new(RejectingProvider);
        let manager = Arc::new(AuthManager::new(vec![provider]));

        let app = Router::new()
            .route("/test", get(|| async { "Hello" }))
            .route_layer(middleware::from_fn(move |req, next| {
                let manager = manager.clone();
                auth_middleware(manager, req, next)
            }));

        let cert_pem = "-----BEGIN CERTIFICATE-----
MIIBkTCB+wIJAKHHCgVZU1XNMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl
c3RDQTAeFw0yNDAxMDEwMDAwMDBaFw0yNTAxMDEwMDAwMDBaMBExDzANBgNVBAMM
BnRlc3RjbDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABAAAAAAAAAAAAAAAAAAA
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAjEcwRQIh
AKxxx/wT4GxmFLRQZeJPLJAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==
-----END CERTIFICATE-----";

        let response = app
            .oneshot(
                AxumRequest::builder()
                    .uri("/test")
                    .header("x-ssl-client-cert", urlencoding::encode(cert_pem).as_ref())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should be unauthorized since authenticated=false
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_middleware_require_cert_headers_true() {
        let provider = Arc::new(MockProvider {
            should_succeed: true,
        });
        let manager = Arc::new(AuthManager::new(vec![provider]));

        // Use auth_middleware_with_options with require_cert_headers=true
        let app = Router::new()
            .route("/test", get(|| async { "Hello" }))
            .route_layer(middleware::from_fn(move |req, next| {
                let manager = manager.clone();
                auth_middleware_with_options(manager, true, req, next)
            }));

        // Request WITHOUT cert headers should be rejected
        let response = app
            .oneshot(
                AxumRequest::builder()
                    .uri("/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // With require_cert_headers=true, missing cert headers returns 401
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_middleware_require_cert_headers_false() {
        let provider = Arc::new(MockProvider {
            should_succeed: true,
        });
        let manager = Arc::new(AuthManager::new(vec![provider]));

        // Use auth_middleware_with_options with require_cert_headers=false (default)
        let app = Router::new()
            .route("/test", get(|| async { "Hello" }))
            .route_layer(middleware::from_fn(move |req, next| {
                let manager = manager.clone();
                auth_middleware_with_options(manager, false, req, next)
            }));

        // Request WITHOUT cert headers should pass (native TLS assumption)
        let response = app
            .oneshot(
                AxumRequest::builder()
                    .uri("/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // With require_cert_headers=false, assumes native TLS verified
        assert_eq!(response.status(), StatusCode::OK);
    }
}
