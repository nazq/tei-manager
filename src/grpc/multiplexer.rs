//! TeiMultiplexer service implementation - routes requests to backend TEI instances

use arrow::array::{Array, ArrayRef, FixedSizeListArray, Float32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use std::io::Cursor;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};
use tracing::{Span, instrument};

use super::pool::BackendPool;
use super::proto::multiplexer::v1 as mux;
use super::proto::tei::v1 as tei;

/// Macro to implement streaming RPC methods
macro_rules! impl_stream_rpc {
    ($self:ident, $request:ident, $mux_req:ty, $backend_client:ident, $backend_method:ident) => {{
        let mut stream: Streaming<$mux_req> = $request.into_inner();

        // Read first request to get instance name
        let first_req: $mux_req = stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("Empty stream"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let instance_name = Self::extract_target(first_req.target.clone())?;
        Span::current().record("instance", instance_name.as_str());

        // Get backend client
        let clients = $self.pool.get_clients(&instance_name).await?;
        let (tx, rx) = tokio::sync::mpsc::channel($self.max_parallel_stream_requests);

        // Spawn task to handle streaming
        tokio::spawn(async move {
            // Create backend request stream
            let backend_stream = async_stream::stream! {
                if let Some(req) = first_req.request {
                    yield req;
                }
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(req) => {
                            if let Some(inner) = req.request {
                                yield inner;
                            }
                        }
                        Err(e) => {
                            tracing::error!("Stream error: {}", e);
                            break;
                        }
                    }
                }
            };

            // Call backend with stream
            let response_stream = match clients
                .$backend_client
                .clone()
                .$backend_method(backend_stream)
                .await
            {
                Ok(response) => response.into_inner(),
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };

            // Forward responses to client
            tokio::pin!(response_stream);
            while let Some(result) = response_stream.next().await {
                if tx.send(result).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }};
}

/// TeiMultiplexer service implementation
#[derive(Clone)]
pub struct TeiMultiplexerService {
    pool: BackendPool,
    max_parallel_stream_requests: usize,
}

impl TeiMultiplexerService {
    pub fn new(pool: BackendPool, max_parallel_stream_requests: usize) -> Self {
        Self {
            pool,
            max_parallel_stream_requests,
        }
    }

    /// Extract target instance from request
    fn extract_target(target: Option<mux::Target>) -> Result<String, Status> {
        let target = target.ok_or_else(|| Status::invalid_argument("Missing target"))?;

        match target.routing {
            Some(mux::target::Routing::InstanceName(name)) => {
                if name.is_empty() {
                    return Err(Status::invalid_argument("Instance name cannot be empty"));
                }
                Ok(name)
            }
            Some(mux::target::Routing::ModelId(_)) => {
                // TODO: Auto-select instance by model
                Err(Status::unimplemented(
                    "Model-based routing not yet implemented",
                ))
            }
            Some(mux::target::Routing::InstanceIndex(_)) => {
                // TODO: Index-based routing
                Err(Status::unimplemented(
                    "Index-based routing not yet implemented",
                ))
            }
            None => Err(Status::invalid_argument("No routing specified")),
        }
    }
}

#[tonic::async_trait]
impl mux::tei_multiplexer_server::TeiMultiplexer for TeiMultiplexerService {
    // ========================================================================
    // Info Service
    // ========================================================================

    #[instrument(skip(self, request), fields(instance))]
    async fn info(
        &self,
        request: Request<mux::InfoRequest>,
    ) -> Result<Response<tei::InfoResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        // Record instance name in span for tracing
        Span::current().record("instance", instance_name.as_str());

        // Get backend client (lock-free lookup)
        let clients = self.pool.get_clients(&instance_name).await?;

        // Forward request to backend (zero-copy - just passes through)
        let response = clients.info.clone().info(tei::InfoRequest {}).await?;

        Ok(response)
    }

    // ========================================================================
    // Embed Service - Unary RPCs
    // ========================================================================

    #[instrument(skip(self, request), fields(instance, inputs_len))]
    async fn embed(
        &self,
        request: Request<mux::EmbedRequest>,
    ) -> Result<Response<tei::EmbedResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        // Extract inner request
        let embed_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing embed request"))?;

        // Record metrics
        Span::current()
            .record("instance", instance_name.as_str())
            .record("inputs_len", embed_req.inputs.len());

        // Get backend client
        let clients = self.pool.get_clients(&instance_name).await?;

        // Forward to backend
        let response = clients.embed.clone().embed(embed_req).await?;

        Ok(response)
    }

    #[instrument(skip(self, request), fields(instance))]
    async fn embed_sparse(
        &self,
        request: Request<mux::EmbedSparseRequest>,
    ) -> Result<Response<tei::EmbedSparseResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing embed_sparse request"))?;

        Span::current().record("instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = clients.embed.clone().embed_sparse(inner_req).await?;

        Ok(response)
    }

    #[instrument(skip(self, request), fields(instance))]
    async fn embed_all(
        &self,
        request: Request<mux::EmbedAllRequest>,
    ) -> Result<Response<tei::EmbedAllResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing embed_all request"))?;

        Span::current().record("instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = clients.embed.clone().embed_all(inner_req).await?;

        Ok(response)
    }

    // ========================================================================
    // Embed Service - Streaming RPCs
    // ========================================================================

    type EmbedStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::EmbedResponse, Status>>;

    #[instrument(skip(self, request), fields(instance))]
    async fn embed_stream(
        &self,
        request: Request<Streaming<mux::EmbedRequest>>,
    ) -> Result<Response<Self::EmbedStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::EmbedRequest, embed, embed_stream)
    }

    type EmbedSparseStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::EmbedSparseResponse, Status>>;

    #[instrument(skip(self, request), fields(instance))]
    async fn embed_sparse_stream(
        &self,
        request: Request<Streaming<mux::EmbedSparseRequest>>,
    ) -> Result<Response<Self::EmbedSparseStreamStream>, Status> {
        impl_stream_rpc!(
            self,
            request,
            mux::EmbedSparseRequest,
            embed,
            embed_sparse_stream
        )
    }

    type EmbedAllStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::EmbedAllResponse, Status>>;

    #[instrument(skip(self, request), fields(instance))]
    async fn embed_all_stream(
        &self,
        request: Request<Streaming<mux::EmbedAllRequest>>,
    ) -> Result<Response<Self::EmbedAllStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::EmbedAllRequest, embed, embed_all_stream)
    }

    // ========================================================================
    // Predict Service
    // ========================================================================

    #[instrument(skip(self, request), fields(instance))]
    async fn predict(
        &self,
        request: Request<mux::PredictRequest>,
    ) -> Result<Response<tei::PredictResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing predict request"))?;

        Span::current().record("instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = clients.predict.clone().predict(inner_req).await?;

        Ok(response)
    }

    #[instrument(skip(self, request), fields(instance))]
    async fn predict_pair(
        &self,
        request: Request<mux::PredictPairRequest>,
    ) -> Result<Response<tei::PredictResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing predict_pair request"))?;

        Span::current().record("instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = clients.predict.clone().predict_pair(inner_req).await?;

        Ok(response)
    }

    type PredictStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::PredictResponse, Status>>;

    #[instrument(skip(self, request), fields(instance))]
    async fn predict_stream(
        &self,
        request: Request<Streaming<mux::PredictRequest>>,
    ) -> Result<Response<Self::PredictStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::PredictRequest, predict, predict_stream)
    }

    type PredictPairStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::PredictResponse, Status>>;

    #[instrument(skip(self, request), fields(instance))]
    async fn predict_pair_stream(
        &self,
        request: Request<Streaming<mux::PredictPairRequest>>,
    ) -> Result<Response<Self::PredictPairStreamStream>, Status> {
        impl_stream_rpc!(
            self,
            request,
            mux::PredictPairRequest,
            predict,
            predict_pair_stream
        )
    }

    // ========================================================================
    // Rerank Service
    // ========================================================================

    #[instrument(skip(self, request), fields(instance))]
    async fn rerank(
        &self,
        request: Request<mux::RerankRequest>,
    ) -> Result<Response<tei::RerankResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing rerank request"))?;

        Span::current().record("instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = clients.rerank.clone().rerank(inner_req).await?;

        Ok(response)
    }

    #[instrument(skip(self, request), fields(instance))]
    async fn rerank_stream(
        &self,
        request: Request<Streaming<mux::RerankStreamRequest>>,
    ) -> Result<Response<tei::RerankResponse>, Status> {
        let mut stream = request.into_inner();

        let first_req = stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("Empty stream"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let instance_name = Self::extract_target(first_req.target)?;
        Span::current().record("instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;

        // Create backend request stream
        let backend_stream = async_stream::stream! {
            if let Some(req) = first_req.request {
                yield req;
            }
            while let Some(result) = stream.next().await {
                match result {
                    Ok(req) => {
                        if let Some(inner) = req.request {
                            yield inner;
                        }
                    }
                    Err(e) => {
                        tracing::error!("Stream error: {}", e);
                        break;
                    }
                }
            }
        };

        // RerankStream returns single response (not streaming)
        let response = clients.rerank.clone().rerank_stream(backend_stream).await?;

        Ok(response)
    }

    // ========================================================================
    // Tokenize Service
    // ========================================================================

    #[instrument(skip(self, request), fields(instance))]
    async fn tokenize(
        &self,
        request: Request<mux::EncodeRequest>,
    ) -> Result<Response<tei::EncodeResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing tokenize request"))?;

        Span::current().record("instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = clients.tokenize.clone().tokenize(inner_req).await?;

        Ok(response)
    }

    type TokenizeStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::EncodeResponse, Status>>;

    #[instrument(skip(self, request), fields(instance))]
    async fn tokenize_stream(
        &self,
        request: Request<Streaming<mux::EncodeRequest>>,
    ) -> Result<Response<Self::TokenizeStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::EncodeRequest, tokenize, tokenize_stream)
    }

    #[instrument(skip(self, request), fields(instance))]
    async fn decode(
        &self,
        request: Request<mux::DecodeRequest>,
    ) -> Result<Response<tei::DecodeResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing decode request"))?;

        Span::current().record("instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = clients.tokenize.clone().decode(inner_req).await?;

        Ok(response)
    }

    type DecodeStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::DecodeResponse, Status>>;

    #[instrument(skip(self, request), fields(instance))]
    async fn decode_stream(
        &self,
        request: Request<Streaming<mux::DecodeRequest>>,
    ) -> Result<Response<Self::DecodeStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::DecodeRequest, tokenize, decode_stream)
    }

    // ========================================================================
    // Arrow Batch Embedding
    // ========================================================================

    #[instrument(skip(self, request), fields(instance, num_rows))]
    async fn embed_arrow(
        &self,
        request: Request<mux::EmbedArrowRequest>,
    ) -> Result<Response<mux::EmbedArrowResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;

        Span::current().record("instance", instance_name.as_str());

        // Deserialize Arrow RecordBatch
        let cursor = Cursor::new(&req.arrow_ipc);
        let mut reader = StreamReader::try_new(cursor, None)
            .map_err(|e| Status::invalid_argument(format!("Invalid Arrow IPC: {}", e)))?;

        let batch = reader
            .next()
            .ok_or_else(|| Status::invalid_argument("No RecordBatch in stream"))?
            .map_err(|e| Status::invalid_argument(format!("Failed to read RecordBatch: {}", e)))?;

        Span::current().record("num_rows", batch.num_rows());

        // Extract text column
        let text_array = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| Status::invalid_argument("First column must be StringArray"))?;

        // Check if noop mode (for round-trip testing)
        let embedding_len: i32;
        let all_embeddings: Vec<Vec<f32>>;

        if req.noop {
            // Noop mode: return dummy embeddings instantly
            embedding_len = 384; // Standard BGE-small embedding size
            all_embeddings = (0..text_array.len()).map(|_| vec![0.0f32; 384]).collect();
        } else {
            // Normal mode: use gRPC streaming for efficiency
            let clients = self.pool.get_clients(&instance_name).await?;

            // Extract texts into Vec (avoiding repeated string allocations)
            let texts: Vec<String> = (0..text_array.len())
                .filter(|&i| !text_array.is_null(i))
                .map(|i| text_array.value(i).to_string())
                .collect();

            // Create stream of EmbedRequest messages
            let truncate = req.truncate;
            let normalize = req.normalize;

            let request_stream = async_stream::stream! {
                for text in texts.iter() {
                    yield tei::EmbedRequest {
                        inputs: text.clone(),
                        truncate,
                        normalize,
                        truncation_direction: 0,
                        prompt_name: None,
                        dimensions: None,
                    };
                }
            };

            // Call TEI's embed_stream (batched streaming)
            let mut response_stream = clients
                .embed
                .clone()
                .embed_stream(request_stream)
                .await
                .map_err(|e| Status::internal(format!("embed_stream failed: {}", e)))?
                .into_inner();

            // Collect responses in order
            let mut embeddings_vec = Vec::new();
            let mut emb_len = None;

            while let Some(result) = response_stream.next().await {
                let response = result
                    .map_err(|e| Status::internal(format!("Stream response error: {}", e)))?;

                if emb_len.is_none() {
                    emb_len = Some(response.embeddings.len() as i32);
                }

                embeddings_vec.push(response.embeddings);
            }

            embedding_len = emb_len.unwrap_or(384);
            all_embeddings = embeddings_vec;
        }

        // Build Arrow RecordBatch with embeddings
        let flat_embeddings: Vec<f32> = all_embeddings.into_iter().flatten().collect();
        let values = Arc::new(Float32Array::from(flat_embeddings)) as ArrayRef;

        let field = Arc::new(Field::new("item", DataType::Float32, false));
        let embeddings_array = FixedSizeListArray::new(field, embedding_len, values, None);

        let schema = Arc::new(Schema::new(vec![Field::new(
            "embeddings",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, false)),
                embedding_len,
            ),
            false,
        )]));

        let result_batch =
            RecordBatch::try_new(schema, vec![Arc::new(embeddings_array) as ArrayRef])
                .map_err(|e| Status::internal(format!("Failed to create RecordBatch: {}", e)))?;

        // Serialize to Arrow IPC with LZ4 compression
        let mut buffer = Vec::new();
        {
            use arrow::ipc::CompressionType;
            use arrow::ipc::writer::IpcWriteOptions;

            let write_options = IpcWriteOptions::default()
                .try_with_compression(Some(CompressionType::LZ4_FRAME))
                .map_err(|e| Status::internal(format!("Failed to set compression: {}", e)))?;

            let mut writer = StreamWriter::try_new_with_options(
                &mut buffer,
                &result_batch.schema(),
                write_options,
            )
            .map_err(|e| Status::internal(format!("Failed to create IPC writer: {}", e)))?;

            writer
                .write(&result_batch)
                .map_err(|e| Status::internal(format!("Failed to write RecordBatch: {}", e)))?;

            writer
                .finish()
                .map_err(|e| Status::internal(format!("Failed to finish IPC writer: {}", e)))?;
        }

        Ok(Response::new(mux::EmbedArrowResponse { arrow_ipc: buffer }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InstanceConfig;
    use crate::registry::Registry;
    use std::sync::Arc;
    use tonic::Code;

    // Import the trait to call RPC methods
    use mux::tei_multiplexer_server::TeiMultiplexer;

    fn create_test_service() -> TeiMultiplexerService {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);
        TeiMultiplexerService::new(pool, 1024)
    }

    async fn add_test_instance(registry: &Arc<Registry>, name: &str, port: u16) {
        let config = InstanceConfig {
            name: name.to_string(),
            model_id: "test-model".to_string(),
            port,
            max_batch_tokens: 1024,
            max_concurrent_requests: 10,
            pooling: None,
            gpu_id: None,
            prometheus_port: None,
            ..Default::default()
        };
        registry.add(config).await.unwrap();
    }

    // ========================================================================
    // Target Extraction Tests
    // ========================================================================

    #[test]
    fn test_extract_target_valid_instance_name() {
        let target = Some(mux::Target {
            routing: Some(mux::target::Routing::InstanceName(
                "test-instance".to_string(),
            )),
        });
        let result = TeiMultiplexerService::extract_target(target);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "test-instance");
    }

    #[test]
    fn test_extract_target_empty_instance_name() {
        let target = Some(mux::Target {
            routing: Some(mux::target::Routing::InstanceName("".to_string())),
        });
        let result = TeiMultiplexerService::extract_target(target);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("cannot be empty"));
    }

    #[test]
    fn test_extract_target_missing() {
        let result = TeiMultiplexerService::extract_target(None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing target"));
    }

    #[test]
    fn test_extract_target_no_routing() {
        let target = Some(mux::Target { routing: None });
        let result = TeiMultiplexerService::extract_target(target);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("No routing specified"));
    }

    #[test]
    fn test_extract_target_model_routing_unimplemented() {
        let target = Some(mux::Target {
            routing: Some(mux::target::Routing::ModelId("bert-base".to_string())),
        });
        let result = TeiMultiplexerService::extract_target(target);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::Unimplemented);
        assert!(
            err.message()
                .contains("Model-based routing not yet implemented")
        );
    }

    #[test]
    fn test_extract_target_index_routing_unimplemented() {
        let target = Some(mux::Target {
            routing: Some(mux::target::Routing::InstanceIndex(0)),
        });
        let result = TeiMultiplexerService::extract_target(target);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::Unimplemented);
        assert!(
            err.message()
                .contains("Index-based routing not yet implemented")
        );
    }

    // ========================================================================
    // Info RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_info_missing_target() {
        let service = create_test_service();
        let request = Request::new(mux::InfoRequest { target: None });
        let result = service.info(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_info_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::InfoRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
        });
        let result = service.info(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    #[tokio::test]
    async fn test_info_instance_not_running() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry.clone());
        let service = TeiMultiplexerService::new(pool, 1024);

        add_test_instance(&registry, "stopped-instance", 59999).await;

        let request = Request::new(mux::InfoRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "stopped-instance".to_string(),
                )),
            }),
        });
        let result = service.info(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::Unavailable);
    }

    // ========================================================================
    // Embed RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_embed_missing_target() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedRequest {
            target: None,
            request: Some(tei::EmbedRequest {
                inputs: "test".to_string(),
                truncate: false,
                normalize: false,
                truncation_direction: tei::TruncationDirection::Right as i32,
                prompt_name: None,
                dimensions: None,
            }),
        });
        let result = service.embed(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_embed_missing_request() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            request: None,
        });
        let result = service.embed(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing embed request"));
    }

    #[tokio::test]
    async fn test_embed_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            request: Some(tei::EmbedRequest {
                inputs: "test".to_string(),
                truncate: false,
                normalize: false,
                truncation_direction: tei::TruncationDirection::Right as i32,
                prompt_name: None,
                dimensions: None,
            }),
        });
        let result = service.embed(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    // ========================================================================
    // EmbedSparse RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_embed_sparse_missing_request() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedSparseRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            request: None,
        });
        let result = service.embed_sparse(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing embed_sparse request"));
    }

    #[tokio::test]
    async fn test_embed_sparse_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedSparseRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            request: Some(tei::EmbedSparseRequest {
                inputs: "test".to_string(),
                truncate: false,
                truncation_direction: tei::TruncationDirection::Right as i32,
                prompt_name: None,
            }),
        });
        let result = service.embed_sparse(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    // ========================================================================
    // EmbedAll RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_embed_all_missing_request() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedAllRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            request: None,
        });
        let result = service.embed_all(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing embed_all request"));
    }

    // ========================================================================
    // Predict RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_predict_missing_request() {
        let service = create_test_service();
        let request = Request::new(mux::PredictRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            request: None,
        });
        let result = service.predict(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing predict request"));
    }

    #[tokio::test]
    async fn test_predict_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::PredictRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            request: Some(tei::PredictRequest {
                inputs: "test".to_string(),
                truncate: false,
                raw_scores: false,
                truncation_direction: tei::TruncationDirection::Right as i32,
            }),
        });
        let result = service.predict(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    // ========================================================================
    // PredictPair RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_predict_pair_missing_request() {
        let service = create_test_service();
        let request = Request::new(mux::PredictPairRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            request: None,
        });
        let result = service.predict_pair(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing predict_pair request"));
    }

    // ========================================================================
    // Rerank RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_rerank_missing_request() {
        let service = create_test_service();
        let request = Request::new(mux::RerankRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            request: None,
        });
        let result = service.rerank(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing rerank request"));
    }

    #[tokio::test]
    async fn test_rerank_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::RerankRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            request: Some(tei::RerankRequest {
                query: "test query".to_string(),
                texts: vec!["doc1".to_string(), "doc2".to_string()],
                truncate: false,
                raw_scores: false,
                return_text: false,
                truncation_direction: tei::TruncationDirection::Right as i32,
            }),
        });
        let result = service.rerank(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    // ========================================================================
    // Tokenize RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_tokenize_missing_request() {
        let service = create_test_service();
        let request = Request::new(mux::EncodeRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            request: None,
        });
        let result = service.tokenize(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing tokenize request"));
    }

    #[tokio::test]
    async fn test_tokenize_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::EncodeRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            request: Some(tei::EncodeRequest {
                inputs: "test".to_string(),
                add_special_tokens: true,
                prompt_name: None,
            }),
        });
        let result = service.tokenize(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    // ========================================================================
    // Decode RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_decode_missing_request() {
        let service = create_test_service();
        let request = Request::new(mux::DecodeRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            request: None,
        });
        let result = service.decode(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing decode request"));
    }

    // ========================================================================
    // Service Construction Tests
    // ========================================================================

    #[tokio::test]
    async fn test_service_creation() {
        let service = create_test_service();
        assert_eq!(service.max_parallel_stream_requests, 1024);
    }

    #[tokio::test]
    async fn test_service_custom_max_parallel_streams() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);
        let service = TeiMultiplexerService::new(pool, 2048);
        assert_eq!(service.max_parallel_stream_requests, 2048);
    }

    // ========================================================================
    // EmbedAll RPC Tests (Additional)
    // ========================================================================

    #[tokio::test]
    async fn test_embed_all_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedAllRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            request: Some(tei::EmbedAllRequest {
                inputs: "test".to_string(),
                truncate: false,
                truncation_direction: tei::TruncationDirection::Right as i32,
                prompt_name: None,
            }),
        });
        let result = service.embed_all(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    // ========================================================================
    // PredictPair RPC Tests (Additional)
    // ========================================================================

    #[tokio::test]
    async fn test_predict_pair_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::PredictPairRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            request: Some(tei::PredictPairRequest {
                inputs: vec!["test input".to_string(), "test pair".to_string()],
                truncate: false,
                raw_scores: false,
                truncation_direction: tei::TruncationDirection::Right as i32,
            }),
        });
        let result = service.predict_pair(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    // ========================================================================
    // Decode RPC Tests (Additional)
    // ========================================================================

    #[tokio::test]
    async fn test_decode_instance_not_found() {
        let service = create_test_service();
        let request = Request::new(mux::DecodeRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            request: Some(tei::DecodeRequest {
                ids: vec![1, 2, 3],
                skip_special_tokens: true,
            }),
        });
        let result = service.decode(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    // ========================================================================
    // EmbedArrow RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_embed_arrow_missing_target() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedArrowRequest {
            target: None,
            arrow_ipc: vec![],
            truncate: true,
            normalize: true,
            noop: false,
        });
        let result = service.embed_arrow(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_embed_arrow_invalid_ipc() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc: vec![1, 2, 3, 4], // Invalid Arrow IPC bytes
            truncate: true,
            normalize: true,
            noop: false,
        });
        let result = service.embed_arrow(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Invalid Arrow IPC"));
    }

    #[tokio::test]
    async fn test_embed_arrow_empty_ipc() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc: vec![], // Empty Arrow IPC
            truncate: true,
            normalize: true,
            noop: false,
        });
        let result = service.embed_arrow(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_embed_arrow_noop_mode() {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::ipc::writer::StreamWriter;
        use arrow::record_batch::RecordBatch;

        let service = create_test_service();

        // Create valid Arrow IPC with text column
        let text_array = StringArray::from(vec!["Hello", "World"]);
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(text_array) as ArrayRef]).unwrap();

        let mut arrow_ipc = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut arrow_ipc, &schema).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }

        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop: true, // Noop mode - returns dummy embeddings
        });

        let result = service.embed_arrow(request).await;
        assert!(result.is_ok());

        // Verify response has embeddings
        let response = result.unwrap().into_inner();
        assert!(!response.arrow_ipc.is_empty());

        // Decode and verify
        let cursor = std::io::Cursor::new(response.arrow_ipc);
        let mut reader = StreamReader::try_new(cursor, None).unwrap();
        let result_batch = reader.next().unwrap().unwrap();
        assert_eq!(result_batch.num_rows(), 2); // 2 texts -> 2 embeddings
    }

    #[tokio::test]
    async fn test_embed_arrow_wrong_column_type() {
        use arrow::array::Int32Array;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::ipc::writer::StreamWriter;
        use arrow::record_batch::RecordBatch;

        let service = create_test_service();

        // Create Arrow IPC with wrong column type (Int32 instead of String)
        let int_array = Int32Array::from(vec![1, 2, 3]);
        let schema = Arc::new(Schema::new(vec![Field::new(
            "data",
            DataType::Int32,
            false,
        )]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(int_array) as ArrayRef]).unwrap();

        let mut arrow_ipc = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut arrow_ipc, &schema).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }

        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop: true,
        });

        let result = service.embed_arrow(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("StringArray"));
    }

    #[tokio::test]
    async fn test_embed_arrow_instance_not_found() {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::ipc::writer::StreamWriter;
        use arrow::record_batch::RecordBatch;

        let service = create_test_service();

        // Create valid Arrow IPC
        let text_array = StringArray::from(vec!["Hello"]);
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(text_array) as ArrayRef]).unwrap();

        let mut arrow_ipc = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut arrow_ipc, &schema).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }

        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop: false, // Not noop, so it will try to find instance
        });

        let result = service.embed_arrow(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    #[tokio::test]
    async fn test_embed_arrow_noop_empty_batch() {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::ipc::writer::StreamWriter;
        use arrow::record_batch::RecordBatch;

        let service = create_test_service();

        // Create valid Arrow IPC with empty batch
        let text_array = StringArray::from(Vec::<&str>::new());
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(text_array) as ArrayRef]).unwrap();

        let mut arrow_ipc = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut arrow_ipc, &schema).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }

        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop: true,
        });

        let result = service.embed_arrow(request).await;
        assert!(result.is_ok());

        // Verify empty response
        let response = result.unwrap().into_inner();
        let cursor = std::io::Cursor::new(response.arrow_ipc);
        let mut reader = StreamReader::try_new(cursor, None).unwrap();
        let result_batch = reader.next().unwrap().unwrap();
        assert_eq!(result_batch.num_rows(), 0);
    }

    #[tokio::test]
    async fn test_embed_arrow_noop_large_batch() {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::ipc::writer::StreamWriter;
        use arrow::record_batch::RecordBatch;

        let service = create_test_service();

        // Create valid Arrow IPC with many texts
        let texts: Vec<&str> = (0..100).map(|_| "Test text").collect();
        let text_array = StringArray::from(texts);
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(text_array) as ArrayRef]).unwrap();

        let mut arrow_ipc = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut arrow_ipc, &schema).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }

        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop: true,
        });

        let result = service.embed_arrow(request).await;
        assert!(result.is_ok());

        // Verify response
        let response = result.unwrap().into_inner();
        let cursor = std::io::Cursor::new(response.arrow_ipc);
        let mut reader = StreamReader::try_new(cursor, None).unwrap();
        let result_batch = reader.next().unwrap().unwrap();
        assert_eq!(result_batch.num_rows(), 100);
    }

    #[tokio::test]
    async fn test_embed_arrow_noop_verify_embedding_dimensions() {
        use arrow::array::{FixedSizeListArray, Float32Array, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::ipc::writer::StreamWriter;
        use arrow::record_batch::RecordBatch;

        let service = create_test_service();

        // Create valid Arrow IPC
        let text_array = StringArray::from(vec!["Test"]);
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(text_array) as ArrayRef]).unwrap();

        let mut arrow_ipc = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut arrow_ipc, &schema).unwrap();
            writer.write(&batch).unwrap();
            writer.finish().unwrap();
        }

        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop: true,
        });

        let result = service.embed_arrow(request).await;
        assert!(result.is_ok());

        // Verify embedding dimensions (should be 384 for noop mode)
        let response = result.unwrap().into_inner();
        let cursor = std::io::Cursor::new(response.arrow_ipc);
        let mut reader = StreamReader::try_new(cursor, None).unwrap();
        let result_batch = reader.next().unwrap().unwrap();

        // Get embeddings column and verify dimensions
        let embeddings_col = result_batch
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .expect("Should be FixedSizeListArray");

        assert_eq!(embeddings_col.value_length(), 384); // BGE-small embedding size

        // Verify values are all zeros in noop mode
        let values = embeddings_col
            .values()
            .as_any()
            .downcast_ref::<Float32Array>()
            .expect("Should be Float32Array");

        for i in 0..values.len() {
            assert_eq!(values.value(i), 0.0);
        }
    }
}
