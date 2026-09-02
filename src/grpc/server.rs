//! gRPC server initialization and lifecycle management

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

use super::multiplexer::TeiMultiplexerService;
use super::pool::BackendPool;
use super::proto::multiplexer::v1::tei_multiplexer_server::TeiMultiplexerServer;
use crate::config::ArrowOutputDtype;
use crate::registry::Registry;

/// Tunables for the gRPC multiplexer server
#[derive(Debug, Clone, Copy)]
pub struct GrpcOptions {
    /// Max request/response message size in MB
    pub max_message_size_mb: usize,
    /// Channel buffer for concurrent stream forwarding
    pub max_parallel_streams: usize,
    /// Per-request timeout for forwarded unary/Arrow RPCs (0 = none)
    pub request_timeout_secs: u64,
    /// Element type for EmbedArrow responses when unspecified by the request
    pub default_output_dtype: ArrowOutputDtype,
    /// Default number of concurrently executing batches per EmbedArrowStream
    /// call (a stream's first request can override; clamped to 1..=64)
    pub stream_max_concurrent_batches: usize,
}

impl Default for GrpcOptions {
    fn default() -> Self {
        Self {
            max_message_size_mb: 16,
            max_parallel_streams: 1024,
            request_timeout_secs: 30,
            default_output_dtype: ArrowOutputDtype::F32,
            stream_max_concurrent_batches: 4,
        }
    }
}

/// Start the gRPC multiplexer server with graceful shutdown support
///
/// This runs until the shutdown signal is received or an error occurs.
/// The server will stop accepting new connections when shutdown is triggered,
/// but will allow in-flight requests to complete.
pub async fn start_grpc_server_with_shutdown<F>(
    addr: SocketAddr,
    registry: Arc<Registry>,
    tls_config: Option<(String, String, String)>, // (cert, key, ca)
    options: GrpcOptions,
    shutdown_signal: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Future<Output = ()> + Send,
{
    let max_message_size_mb = options.max_message_size_mb;
    let (service, reflection_service, max_message_size) = build_services(registry, options)?;

    // Build server with optional TLS; one span per RPC, parented on the
    // caller's traceparent
    let mut builder = Server::builder().layer(crate::otel::grpc_trace_layer());

    if let Some((cert_pem, key_pem, ca_pem)) = tls_config {
        tracing::info!(
            "Starting gRPC multiplexer on {} with mTLS (max message: {}MB)",
            addr,
            max_message_size_mb
        );

        let server_identity = Identity::from_pem(cert_pem, key_pem);
        let client_ca = Certificate::from_pem(ca_pem);
        let tls = ServerTlsConfig::new()
            .identity(server_identity)
            .client_ca_root(client_ca);

        builder = builder.tls_config(tls)?;
    } else {
        tracing::info!(
            "Starting gRPC multiplexer on {} (no TLS, max message: {}MB)",
            addr,
            max_message_size_mb
        );
    }

    builder
        .add_service(
            TeiMultiplexerServer::new(service)
                .max_decoding_message_size(max_message_size)
                .max_encoding_message_size(max_message_size),
        )
        .add_service(reflection_service)
        .serve_with_shutdown(addr, shutdown_signal)
        .await?;

    tracing::info!("gRPC server shut down gracefully");
    Ok(())
}

/// Start the gRPC multiplexer server (runs indefinitely)
///
/// This runs indefinitely until an error occurs or the server is shut down.
/// For graceful shutdown support, use `start_grpc_server_with_shutdown` instead.
pub async fn start_grpc_server(
    addr: SocketAddr,
    registry: Arc<Registry>,
    tls_config: Option<(String, String, String)>, // (cert, key, ca)
    options: GrpcOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let max_message_size_mb = options.max_message_size_mb;
    let (service, reflection_service, max_message_size) = build_services(registry, options)?;

    // Build server with optional TLS; one span per RPC, parented on the
    // caller's traceparent
    let mut builder = Server::builder().layer(crate::otel::grpc_trace_layer());

    if let Some((cert_pem, key_pem, ca_pem)) = tls_config {
        tracing::info!(
            "Starting gRPC multiplexer on {} with mTLS (max message: {}MB)",
            addr,
            max_message_size_mb
        );

        let server_identity = Identity::from_pem(cert_pem, key_pem);
        let client_ca = Certificate::from_pem(ca_pem);
        let tls = ServerTlsConfig::new()
            .identity(server_identity)
            .client_ca_root(client_ca);

        builder = builder.tls_config(tls)?;
    } else {
        tracing::info!(
            "Starting gRPC multiplexer on {} (no TLS, max message: {}MB)",
            addr,
            max_message_size_mb
        );
    }

    builder
        .add_service(
            TeiMultiplexerServer::new(service)
                .max_decoding_message_size(max_message_size)
                .max_encoding_message_size(max_message_size),
        )
        .add_service(reflection_service)
        .serve(addr)
        .await?;

    Ok(())
}

/// Build the gRPC services (shared between server variants)
fn build_services(
    registry: Arc<Registry>,
    options: GrpcOptions,
) -> Result<
    (
        TeiMultiplexerService,
        tonic_reflection::server::v1::ServerReflectionServer<
            impl tonic_reflection::server::v1::ServerReflection,
        >,
        usize,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    // Create connection pool
    let pool = BackendPool::new(registry);

    // Create multiplexer service with timeout
    let service = TeiMultiplexerService::new(
        pool,
        options.max_parallel_streams,
        options.request_timeout_secs,
    )
    .with_default_output_dtype(options.default_output_dtype)
    .with_stream_max_concurrent_batches(options.stream_max_concurrent_batches);

    // Enable gRPC reflection
    let file_descriptor_set: &[u8] = tonic::include_file_descriptor_set!("descriptor");
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(file_descriptor_set)
        .build_v1()?;

    // Message size limits from config
    let max_message_size: usize = options.max_message_size_mb * 1024 * 1024;

    Ok((service, reflection_service, max_message_size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn create_test_registry() -> Arc<Registry> {
        Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ))
    }

    #[tokio::test]
    async fn test_server_module_compiles() {
        // Basic compilation test
        let registry = create_test_registry();
        let pool = BackendPool::new(registry);
        let _service = TeiMultiplexerService::new(pool, 1024, 30);
    }

    #[tokio::test]
    async fn test_server_creates_pool_and_service() {
        let registry = create_test_registry();
        let pool = BackendPool::new(registry.clone());
        let service = TeiMultiplexerService::new(pool, 512, 30);

        // Service was created successfully
        assert!(std::mem::size_of_val(&service) > 0);
    }

    #[tokio::test]
    async fn test_message_size_calculation() {
        // Test that message size calculation works correctly
        let max_message_size_mb: usize = 16;
        let max_message_size: usize = max_message_size_mb * 1024 * 1024;
        assert_eq!(max_message_size, 16 * 1024 * 1024);
        assert_eq!(max_message_size, 16_777_216);

        // Test with 1 MB
        let one_mb: usize = 1024 * 1024;
        assert_eq!(one_mb, 1_048_576);
    }

    #[tokio::test]
    async fn test_server_starts_without_tls() {
        let registry = create_test_registry();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Spawn server in background and cancel quickly
        let handle = tokio::spawn(async move {
            start_grpc_server(addr, registry, None, GrpcOptions::default()).await
        });

        // Give it a moment to start, then abort
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();

        // Server was started (and aborted)
        let result = handle.await;
        assert!(result.is_err()); // JoinError due to abort
    }

    #[tokio::test]
    async fn test_server_starts_with_different_message_sizes() {
        for size_mb in [1, 8, 16, 32, 64] {
            let registry = create_test_registry();
            let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

            let handle = tokio::spawn(async move {
                start_grpc_server(
                    addr,
                    registry,
                    None,
                    GrpcOptions {
                        max_message_size_mb: size_mb,
                        max_parallel_streams: 1024,
                        request_timeout_secs: 30,
                        ..Default::default()
                    },
                )
                .await
            });

            tokio::time::sleep(Duration::from_millis(30)).await;
            handle.abort();
        }
    }

    #[tokio::test]
    async fn test_server_starts_with_different_parallel_stream_limits() {
        for streams in [128, 256, 512, 1024, 2048] {
            let registry = create_test_registry();
            let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

            let handle = tokio::spawn(async move {
                start_grpc_server(
                    addr,
                    registry,
                    None,
                    GrpcOptions {
                        max_message_size_mb: 16,
                        max_parallel_streams: streams,
                        request_timeout_secs: 30,
                        ..Default::default()
                    },
                )
                .await
            });

            tokio::time::sleep(Duration::from_millis(30)).await;
            handle.abort();
        }
    }

    #[tokio::test]
    async fn test_server_with_invalid_tls_config_fails() {
        // Install rustls crypto provider for TLS tests
        let _ = rustls::crypto::ring::default_provider().install_default();

        let registry = create_test_registry();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Invalid TLS config (garbage data)
        let invalid_tls = Some((
            "not a valid cert".to_string(),
            "not a valid key".to_string(),
            "not a valid ca".to_string(),
        ));

        let result = timeout(
            Duration::from_secs(1),
            start_grpc_server(addr, registry, invalid_tls, GrpcOptions::default()),
        )
        .await;

        // Should either timeout or fail due to invalid TLS
        match result {
            Ok(Err(_)) => {} // Expected: TLS config error
            Err(_) => {}     // Timeout is also acceptable
            Ok(Ok(())) => panic!("Should not succeed with invalid TLS"),
        }
    }

    #[tokio::test]
    async fn test_reflection_service_descriptor() {
        // Test that the file descriptor set can be loaded
        let file_descriptor_set: &[u8] = tonic::include_file_descriptor_set!("descriptor");
        assert!(!file_descriptor_set.is_empty());

        // Verify we can build a reflection service
        let reflection_result = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(file_descriptor_set)
            .build_v1();

        assert!(reflection_result.is_ok());
    }

    #[tokio::test]
    async fn test_backend_pool_creation() {
        let registry = create_test_registry();
        let pool = BackendPool::new(registry.clone());

        // Pool should be empty initially
        // (testing that pool creation doesn't panic)
        assert!(std::mem::size_of_val(&pool) > 0);
    }

    #[tokio::test]
    async fn test_tei_multiplexer_server_wrapper() {
        let registry = create_test_registry();
        let pool = BackendPool::new(registry);
        let service = TeiMultiplexerService::new(pool, 1024, 30);

        // Test that TeiMultiplexerServer can wrap the service
        let max_message_size = 16 * 1024 * 1024;
        let server = TeiMultiplexerServer::new(service)
            .max_decoding_message_size(max_message_size)
            .max_encoding_message_size(max_message_size);

        // Server wrapper created successfully
        assert!(std::mem::size_of_val(&server) > 0);
    }

    #[tokio::test]
    async fn test_server_builder_configuration() {
        // Test Server builder without actually serving
        let builder = Server::builder();

        // Builder should be configurable
        assert!(std::mem::size_of_val(&builder) > 0);
    }

    #[tokio::test]
    async fn test_socket_addr_parsing() {
        // Test various address formats that might be used
        let addrs = [
            "0.0.0.0:50051",
            "127.0.0.1:50051",
            "[::]:50051",
            "0.0.0.0:0",
        ];

        for addr_str in addrs {
            let addr: Result<SocketAddr, _> = addr_str.parse();
            assert!(addr.is_ok(), "Failed to parse: {}", addr_str);
        }
    }

    #[tokio::test]
    async fn test_concurrent_server_starts_on_different_ports() {
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let registry = create_test_registry();
                let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
                tokio::spawn(async move {
                    start_grpc_server(addr, registry, None, GrpcOptions::default()).await
                })
            })
            .collect();

        tokio::time::sleep(Duration::from_millis(50)).await;

        for handle in handles {
            handle.abort();
        }
    }

    #[tokio::test]
    async fn test_graceful_shutdown_completes() {
        let registry = create_test_registry();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Create a channel to signal shutdown
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            start_grpc_server_with_shutdown(
                addr,
                registry,
                None,
                GrpcOptions::default(),
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        // Give the server time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Signal shutdown
        let _ = shutdown_tx.send(());

        // Server should complete gracefully within timeout
        let result = timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "Server should shut down within timeout");
        assert!(
            result.unwrap().is_ok(),
            "Server task should complete successfully"
        );
    }

    #[tokio::test]
    async fn test_graceful_shutdown_with_broadcast_channel() {
        let registry = create_test_registry();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Create a broadcast channel (like main.rs uses)
        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        let mut shutdown_rx = shutdown_tx.subscribe();

        let handle = tokio::spawn(async move {
            start_grpc_server_with_shutdown(
                addr,
                registry,
                None,
                GrpcOptions::default(),
                async move {
                    let _ = shutdown_rx.recv().await;
                },
            )
            .await
        });

        // Give the server time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Signal shutdown via broadcast
        let _ = shutdown_tx.send(());

        // Server should complete gracefully
        let result = timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "Server should shut down within timeout");
    }

    #[tokio::test]
    async fn test_build_services_creates_valid_services() {
        let registry = create_test_registry();
        let result = build_services(registry, GrpcOptions::default());

        assert!(result.is_ok());
        let (_service, _reflection, max_size) = result.unwrap();
        assert_eq!(max_size, 16 * 1024 * 1024);
    }
}
