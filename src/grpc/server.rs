//! gRPC server initialization and lifecycle management

use std::net::SocketAddr;
use std::sync::Arc;
use tonic::transport::Server;

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
) -> Result<(), Box<dyn std::error::Error>> {
    // Create connection pool
    let pool = BackendPool::new(registry);

    // Create multiplexer service
    let service = TeiMultiplexerService::new(pool);

    // Build and start server
    tracing::info!("Starting gRPC multiplexer on {}", addr);

    // Enable gRPC reflection following TEI's pattern (using v1 API for tonic 0.14)
    let file_descriptor_set: &[u8] = tonic::include_file_descriptor_set!("descriptor");
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(file_descriptor_set)
        .build_v1()?;

    Server::builder()
        .add_service(TeiMultiplexerServer::new(service))
        .add_service(reflection_service)
        .serve(addr)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_module_compiles() {
        // Basic compilation test
        let registry = Arc::new(Registry::new(None, "text-embeddings-router".to_string()));
        let pool = BackendPool::new(registry);
        let _service = TeiMultiplexerService::new(pool);
    }
}
