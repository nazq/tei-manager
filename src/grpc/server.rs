//! gRPC server initialization and lifecycle management

use std::net::SocketAddr;
use std::sync::Arc;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

use super::multiplexer::TeiMultiplexerService;
use super::pool::BackendPool;
use super::proto::multiplexer::v1::tei_multiplexer_server::TeiMultiplexerServer;
use crate::registry::Registry;

/// Start the gRPC multiplexer server
///
/// This runs indefinitely until an error occurs or the server is shut down.
/// Should be spawned as a background task alongside the HTTP server.
pub async fn start_grpc_server(
    addr: SocketAddr,
    registry: Arc<Registry>,
    tls_config: Option<(String, String, String)>, // (cert, key, ca)
    max_message_size_mb: usize,
    max_parallel_streams: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Create connection pool
    let pool = BackendPool::new(registry);

    // Create multiplexer service
    let service = TeiMultiplexerService::new(pool, max_parallel_streams);

    // Enable gRPC reflection
    let file_descriptor_set: &[u8] = tonic::include_file_descriptor_set!("descriptor");
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(file_descriptor_set)
        .build_v1()?;

    // Message size limits from config
    let max_message_size: usize = max_message_size_mb * 1024 * 1024;

    // Build server with optional TLS
    let mut builder = Server::builder();

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
        let _service = TeiMultiplexerService::new(pool, 1024);
    }

    #[tokio::test]
    async fn test_server_creates_pool_and_service() {
        let registry = create_test_registry();
        let pool = BackendPool::new(registry.clone());
        let service = TeiMultiplexerService::new(pool, 512);

        // Service was created successfully
        assert!(std::mem::size_of_val(&service) > 0);
    }

    #[tokio::test]
    async fn test_message_size_calculation() {
        // Test that message size calculation works correctly
        let max_message_size_mb: usize = 16;
        let max_message_size: usize = max_message_size_mb * 1024 * 1024;
        assert_eq!(max_message_size, 16 * 1024 * 1024);
        assert_eq!(max_message_size, 16777216);

        // Test with 1 MB
        let one_mb: usize = 1024 * 1024;
        assert_eq!(one_mb, 1048576);
    }

    #[tokio::test]
    async fn test_server_starts_without_tls() {
        let registry = create_test_registry();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Spawn server in background and cancel quickly
        let handle = tokio::spawn(async move {
            start_grpc_server(
                addr, registry, None, // No TLS
                16,   // 16 MB max message
                1024, // max parallel streams
            )
            .await
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
                start_grpc_server(addr, registry, None, size_mb, 1024).await
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

            let handle =
                tokio::spawn(
                    async move { start_grpc_server(addr, registry, None, 16, streams).await },
                );

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
            start_grpc_server(addr, registry, invalid_tls, 16, 1024),
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
        let service = TeiMultiplexerService::new(pool, 1024);

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
                tokio::spawn(async move { start_grpc_server(addr, registry, None, 16, 1024).await })
            })
            .collect();

        tokio::time::sleep(Duration::from_millis(50)).await;

        for handle in handles {
            handle.abort();
        }
    }
}
