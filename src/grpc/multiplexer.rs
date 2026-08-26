//! TeiMultiplexer service implementation - routes requests to backend TEI instances

use arrow::array::{
    ArrayRef, FixedSizeListArray, Float32Array, ListArray, StringArray, StructArray, UInt32Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Fields, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};
use tracing::{Span, instrument};

use super::pool::BackendPool;
use super::proto::multiplexer::v1 as mux;
use super::proto::tei::v1 as tei;
use crate::config::ArrowOutputDtype;

/// Implements a bidirectional streaming RPC method for the multiplexer.
///
/// This macro generates the boilerplate for forwarding gRPC streaming calls
/// to the appropriate backend TEI instance. It handles:
///
/// 1. **Request extraction**: Reads the first request to get the target instance name
/// 2. **Instance routing**: Looks up the backend connection from the connection pool
/// 3. **Stream forwarding**: Spawns a task to forward requests to the backend
/// 4. **Response streaming**: Returns responses from the backend via a channel
///
/// # Arguments
///
/// * `$self` - The service instance (`&self`)
/// * `$request` - The incoming tonic `Request<Streaming<MuxRequest>>`
/// * `$mux_req` - The multiplexer request type (e.g., `mux::EmbedRequest`)
/// * `$backend_client` - The client field name on `TeiClients` (e.g., `embed`, `predict`)
/// * `$backend_method` - The method name to call on the backend client (e.g., `embed_stream`)
///
/// # Generated Flow
///
/// ```text
/// Client Request Stream
///        │
///        ▼
/// ┌──────────────┐
/// │ Read First   │──► Extract target instance name
/// │ Request      │
/// └──────────────┘
///        │
///        ▼
/// ┌──────────────┐
/// │ Get Backend  │──► Lock-free lookup in connection pool
/// │ Clients      │
/// └──────────────┘
///        │
///        ▼
/// ┌──────────────┐
/// │ Spawn Async  │──► Forward stream to backend
/// │ Task         │
/// └──────────────┘
///        │
///        ▼
/// Client Response Stream ◄── Backend responses via mpsc channel
/// ```
///
/// # Example Usage
///
/// ```rust,ignore
/// async fn embed_stream(
///     &self,
///     request: Request<Streaming<mux::EmbedRequest>>,
/// ) -> Result<Response<Self::EmbedStreamStream>, Status> {
///     impl_stream_rpc!(self, request, mux::EmbedRequest, embed, embed_stream)
/// }
/// ```
///
/// # Error Handling
///
/// - Returns `InvalidArgument` if the stream is empty
/// - Returns `NotFound` if the target instance doesn't exist
/// - Returns `Unavailable` if the backend connection fails
/// - Stream errors are logged and terminate the forwarding task
macro_rules! impl_stream_rpc {
    ($self:ident, $request:ident, $mux_req:ty, $backend_client:ident, $backend_method:ident) => {{
        let mut stream: Streaming<$mux_req> = $request.into_inner();

        // Read first request to get instance name
        let first_req: $mux_req = stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("Empty stream"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let instance_name = Self::extract_target(first_req.target)?;
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
    request_timeout: Option<Duration>,
    /// Element type for EmbedArrow responses when the request says UNSPECIFIED
    default_output_dtype: ArrowOutputDtype,
}

impl TeiMultiplexerService {
    pub fn new(
        pool: BackendPool,
        max_parallel_stream_requests: usize,
        request_timeout_secs: u64,
    ) -> Self {
        Self {
            pool,
            max_parallel_stream_requests,
            // 0 means no timeout
            request_timeout: if request_timeout_secs > 0 {
                Some(Duration::from_secs(request_timeout_secs))
            } else {
                None
            },
            default_output_dtype: ArrowOutputDtype::F32,
        }
    }

    /// Set the element type used when a request leaves `output_dtype` unspecified
    pub fn with_default_output_dtype(mut self, dtype: ArrowOutputDtype) -> Self {
        self.default_output_dtype = dtype;
        self
    }

    /// Resolve a request's `output_dtype` against the server default
    fn output_dtype(&self, requested: i32) -> Result<ArrowOutputDtype, Status> {
        match mux::OutputDtype::try_from(requested) {
            Ok(mux::OutputDtype::Unspecified) => Ok(self.default_output_dtype),
            Ok(mux::OutputDtype::F32) => Ok(ArrowOutputDtype::F32),
            Ok(mux::OutputDtype::F16) => Ok(ArrowOutputDtype::F16),
            Err(_) => Err(Status::invalid_argument(format!(
                "unknown output_dtype value {requested}"
            ))),
        }
    }

    /// Wrap a future with an optional timeout
    async fn with_timeout<T, F: std::future::Future<Output = Result<T, Status>>>(
        &self,
        fut: F,
    ) -> Result<T, Status> {
        match self.request_timeout {
            Some(duration) => timeout(duration, fut)
                .await
                .map_err(|_| Status::deadline_exceeded("Request timeout"))?,
            None => fut.await,
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

        // Forward request to backend with timeout
        let response = self
            .with_timeout(async { clients.info.clone().info(tei::InfoRequest {}).await })
            .await?;

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

        // Forward to backend with timeout
        let response = self
            .with_timeout(async { clients.embed.clone().embed(embed_req).await })
            .await?;

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
        let response = self
            .with_timeout(async { clients.embed.clone().embed_sparse(inner_req).await })
            .await?;

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
        let response = self
            .with_timeout(async { clients.embed.clone().embed_all(inner_req).await })
            .await?;

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
        let response = self
            .with_timeout(async { clients.predict.clone().predict(inner_req).await })
            .await?;

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
        let response = self
            .with_timeout(async { clients.predict.clone().predict_pair(inner_req).await })
            .await?;

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
        let response = self
            .with_timeout(async { clients.rerank.clone().rerank(inner_req).await })
            .await?;

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
        let response = self
            .with_timeout(async { clients.tokenize.clone().tokenize(inner_req).await })
            .await?;

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
        let response = self
            .with_timeout(async { clients.tokenize.clone().decode(inner_req).await })
            .await?;

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

        let rows = arrow_batch::parse_text_rows(&req.arrow_ipc)?;
        Span::current().record("num_rows", rows.len());
        let output_dtype = self.output_dtype(req.output_dtype)?;

        let outcome: RowOutcomes<Vec<f32>> = if req.noop {
            RowOutcomes::noop(rows.len(), || vec![0.0f32; NOOP_EMBEDDING_DIM])
        } else {
            let clients = self.pool.get_clients(&instance_name).await?;
            let truncate = req.truncate;
            let normalize = Some(req.normalize);
            let truncation_direction = req.truncation_direction;
            let prompt_name = req.prompt_name.clone();
            let dimensions = req.dimensions;
            let build = |text: &str| tei::EmbedRequest {
                inputs: text.to_string(),
                truncate,
                normalize,
                truncation_direction,
                prompt_name: prompt_name.clone(),
                dimensions,
            };
            let embed = clients.embed.clone();
            self.with_timeout(embed_rows(rows, build, embed, |mut c, s| {
                Box::pin(async move { c.embed_stream(s).await.map(Response::into_inner) })
            }))
            .await?
            .map(|r| r.embeddings)
        };

        let batch = arrow_batch::dense_batch(&outcome, output_dtype)?;
        let buffer = arrow_batch::serialize(&batch, req.compression)?;
        Ok(Response::new(mux::EmbedArrowResponse { arrow_ipc: buffer }))
    }

    #[instrument(skip(self, request), fields(instance, num_rows))]
    async fn embed_sparse_arrow(
        &self,
        request: Request<mux::EmbedSparseArrowRequest>,
    ) -> Result<Response<mux::EmbedSparseArrowResponse>, Status> {
        let req = request.into_inner();
        let instance_name = Self::extract_target(req.target)?;
        Span::current().record("instance", instance_name.as_str());

        let rows = arrow_batch::parse_text_rows(&req.arrow_ipc)?;
        Span::current().record("num_rows", rows.len());

        let outcome: RowOutcomes<Vec<(u32, f32)>> = if req.noop {
            let mut i = 0u32;
            RowOutcomes::noop(rows.len(), || {
                let row = vec![(i, 1.0), (i + 100, 0.5), (i + 200, 0.25)];
                i += 1;
                row
            })
        } else {
            let clients = self.pool.get_clients(&instance_name).await?;
            let truncate = req.truncate;
            let truncation_direction = req.truncation_direction;
            let prompt_name = req.prompt_name.clone();
            let build = |text: &str| tei::EmbedSparseRequest {
                inputs: text.to_string(),
                truncate,
                truncation_direction,
                prompt_name: prompt_name.clone(),
            };
            let embed = clients.embed.clone();
            self.with_timeout(embed_rows(rows, build, embed, |mut c, s| {
                Box::pin(async move { c.embed_sparse_stream(s).await.map(Response::into_inner) })
            }))
            .await?
            .map(|r| {
                r.sparse_embeddings
                    .into_iter()
                    .map(|sv| (sv.index, sv.value))
                    .collect()
            })
        };

        let batch = arrow_batch::sparse_batch(&outcome)?;
        let buffer = arrow_batch::serialize(&batch, req.compression)?;
        Ok(Response::new(mux::EmbedSparseArrowResponse {
            arrow_ipc: buffer,
        }))
    }
}

/// Embedding dimension used by `noop` mode (BGE-small).
const NOOP_EMBEDDING_DIM: usize = 384;

/// Per-row results of a batch, aligned 1:1 with the input rows.
#[derive(Debug, Clone, PartialEq)]
pub struct RowOutcomes<T> {
    pub rows: Vec<Result<T, String>>,
}

impl<T> RowOutcomes<T> {
    fn noop(n: usize, mut make: impl FnMut() -> T) -> Self {
        Self {
            rows: (0..n).map(|_| Ok(make())).collect(),
        }
    }

    fn map<U>(self, f: impl Fn(T) -> U) -> RowOutcomes<U> {
        RowOutcomes {
            rows: self.rows.into_iter().map(|r| r.map(&f)).collect(),
        }
    }

    pub fn ok_count(&self) -> usize {
        self.rows.iter().filter(|r| r.is_ok()).count()
    }
}

/// Error reported for a null input text.
const NULL_INPUT_ERROR: &str = "input text is null";

/// Stream `rows` through a TEI streaming RPC, tolerating per-row failures.
///
/// TEI preserves request order on its streaming RPCs, but tonic terminates a
/// response stream at the first `Err(Status)` — and the client may receive
/// that error *before* the successful responses buffered ahead of it, so the
/// number of responses received does not identify the failing row. After a
/// stream fails with `InvalidArgument` (bad input: empty, too long without
/// `truncate`, ...), the unanswered rows are therefore probed one at a time —
/// a single-row stream can only fail because of that row — until the culprit
/// is found and recorded; bulk streaming then resumes with the rows after it.
///
/// Any other status aborts the whole batch, since it indicates a backend
/// problem rather than a bad row.
async fn embed_rows<Req, Res, C, B, Open, S>(
    rows: Vec<Option<String>>,
    build: B,
    client: C,
    open: Open,
) -> Result<RowOutcomes<Res>, Status>
where
    Req: Send + 'static,
    Res: Send + 'static,
    C: Clone,
    B: Fn(&str) -> Req,
    S: futures::Stream<Item = Result<Res, Status>> + Unpin + Send,
    Open: Fn(
        C,
        tokio_stream::Iter<std::vec::IntoIter<Req>>,
    ) -> futures::future::BoxFuture<'static, Result<S, Status>>,
{
    let mut out: Vec<Option<Result<Res, String>>> = (0..rows.len()).map(|_| None).collect();

    // Indices of rows that actually go to the backend (nulls are pre-failed).
    let mut pending: Vec<usize> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        match row {
            Some(_) => pending.push(i),
            None => out[i] = Some(Err(NULL_INPUT_ERROR.to_string())),
        }
    }

    let mut cursor = 0usize;
    // After a row-level failure, send rows singly until the bad row is found.
    let mut probing = false;
    while cursor < pending.len() {
        let chunk: &[usize] = if probing {
            &pending[cursor..cursor + 1]
        } else {
            &pending[cursor..]
        };
        let requests: Vec<Req> = chunk
            .iter()
            .map(|&i| build(rows[i].as_deref().unwrap_or_default()))
            .collect();

        let mut stream = open(client.clone(), tokio_stream::iter(requests))
            .await
            .map_err(|e| {
                Status::new(e.code(), format!("backend stream failed: {}", e.message()))
            })?;

        let mut received = 0usize;
        let mut failure: Option<Status> = None;
        while let Some(result) = stream.next().await {
            match result {
                Ok(res) if received < chunk.len() => {
                    out[chunk[received]] = Some(Ok(res));
                    received += 1;
                }
                Ok(_) => {
                    return Err(Status::internal(format!(
                        "backend stream returned more than {} responses",
                        chunk.len()
                    )));
                }
                Err(status) => {
                    failure = Some(status);
                    break;
                }
            }
        }

        match failure {
            None if received == chunk.len() => {
                // A successful probe means the bad row is still ahead: keep
                // probing. A successful bulk stream finishes the batch.
                cursor += received;
            }
            None => {
                return Err(Status::internal(format!(
                    "backend stream ended early: expected {} responses, got {}",
                    chunk.len(),
                    received
                )));
            }
            Some(status) if status.code() == tonic::Code::InvalidArgument => {
                if probing {
                    let idx = pending[cursor];
                    tracing::debug!(
                        row = idx,
                        error = status.message(),
                        "row rejected by backend"
                    );
                    out[idx] = Some(Err(status.message().to_string()));
                    cursor += 1;
                    probing = false;
                } else {
                    // Keep whatever arrived; re-probe from the first unanswered row.
                    cursor += received;
                    probing = true;
                }
            }
            Some(status) => {
                return Err(Status::new(
                    status.code(),
                    format!(
                        "backend stream failed after {} of {} rows: {}",
                        cursor + received,
                        pending.len(),
                        status.message()
                    ),
                ));
            }
        }
    }

    Ok(RowOutcomes {
        rows: out
            .into_iter()
            .map(|r| r.expect("every row is resolved"))
            .collect(),
    })
}

/// Arrow IPC parsing and construction for the batch RPCs.
mod arrow_batch {
    use super::*;
    use arrow::array::{Array, BooleanBufferBuilder};
    use arrow::buffer::NullBuffer;
    use arrow::ipc::CompressionType;
    use arrow::ipc::writer::IpcWriteOptions;

    /// Read the first column of the first RecordBatch as text rows (`None` = null).
    pub fn parse_text_rows(ipc: &[u8]) -> Result<Vec<Option<String>>, Status> {
        let mut reader = StreamReader::try_new(Cursor::new(ipc), None)
            .map_err(|e| Status::invalid_argument(format!("Invalid Arrow IPC: {}", e)))?;
        let batch = reader
            .next()
            .ok_or_else(|| Status::invalid_argument("No RecordBatch in stream"))?
            .map_err(|e| Status::invalid_argument(format!("Failed to read RecordBatch: {}", e)))?;
        if batch.num_columns() == 0 {
            return Err(Status::invalid_argument("RecordBatch has no columns"));
        }
        let column = batch.column(0);
        let text = match column.data_type() {
            DataType::Utf8 => column.as_any().downcast_ref::<StringArray>().map(|a| {
                (0..a.len())
                    .map(|i| a.is_valid(i).then(|| a.value(i).to_string()))
                    .collect()
            }),
            DataType::LargeUtf8 => column
                .as_any()
                .downcast_ref::<arrow::array::LargeStringArray>()
                .map(|a| {
                    (0..a.len())
                        .map(|i| a.is_valid(i).then(|| a.value(i).to_string()))
                        .collect()
                }),
            DataType::Utf8View => column
                .as_any()
                .downcast_ref::<arrow::array::StringViewArray>()
                .map(|a| {
                    (0..a.len())
                        .map(|i| a.is_valid(i).then(|| a.value(i).to_string()))
                        .collect()
                }),
            _ => None,
        };
        text.ok_or_else(|| Status::invalid_argument("First column must be StringArray"))
    }

    fn error_column<T>(outcome: &RowOutcomes<T>) -> ArrayRef {
        Arc::new(StringArray::from_iter(
            outcome
                .rows
                .iter()
                .map(|r| r.as_ref().err().map(String::as_str)),
        ))
    }

    fn validity<T>(outcome: &RowOutcomes<T>) -> Option<NullBuffer> {
        if outcome.rows.iter().all(|r| r.is_ok()) {
            return None;
        }
        let mut b = BooleanBufferBuilder::new(outcome.rows.len());
        for r in &outcome.rows {
            b.append(r.is_ok());
        }
        Some(NullBuffer::new(b.finish()))
    }

    /// `embeddings: FixedSizeList<f32|f16>[dim]?`, `error: Utf8?`
    pub fn dense_batch(
        outcome: &RowOutcomes<Vec<f32>>,
        dtype: ArrowOutputDtype,
    ) -> Result<RecordBatch, Status> {
        let dim = outcome
            .rows
            .iter()
            .find_map(|r| r.as_ref().ok().map(Vec::len))
            .unwrap_or(0);
        let mut flat: Vec<f32> = Vec::with_capacity(outcome.rows.len() * dim);
        for r in &outcome.rows {
            match r {
                Ok(v) if v.len() == dim => flat.extend_from_slice(v),
                Ok(v) => {
                    return Err(Status::internal(format!(
                        "inconsistent embedding dimensions: {} vs {}",
                        v.len(),
                        dim
                    )));
                }
                Err(_) => flat.extend(std::iter::repeat_n(0.0f32, dim)),
            }
        }
        let (element, values): (DataType, ArrayRef) = match dtype {
            ArrowOutputDtype::F32 => (DataType::Float32, Arc::new(Float32Array::from(flat))),
            ArrowOutputDtype::F16 => (
                DataType::Float16,
                Arc::new(arrow::array::Float16Array::from_iter_values(
                    flat.into_iter().map(half::f16::from_f32),
                )),
            ),
        };
        let item = Arc::new(Field::new("item", element, false));
        let embeddings =
            FixedSizeListArray::try_new(item.clone(), dim as i32, values, validity(outcome))
                .map_err(|e| {
                    Status::internal(format!("Failed to build embeddings column: {}", e))
                })?;
        let schema = Arc::new(Schema::new(vec![
            Field::new(
                "embeddings",
                DataType::FixedSizeList(item, dim as i32),
                true,
            ),
            Field::new("error", DataType::Utf8, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![Arc::new(embeddings) as ArrayRef, error_column(outcome)],
        )
        .map_err(|e| Status::internal(format!("Failed to create RecordBatch: {}", e)))
    }

    /// `sparse_embeddings: List<Struct<index:u32, value:f32>>?`, `error: Utf8?`
    pub fn sparse_batch(outcome: &RowOutcomes<Vec<(u32, f32)>>) -> Result<RecordBatch, Status> {
        let struct_fields = Fields::from(vec![
            Field::new("index", DataType::UInt32, false),
            Field::new("value", DataType::Float32, false),
        ]);
        let mut offsets: Vec<i32> = Vec::with_capacity(outcome.rows.len() + 1);
        offsets.push(0);
        let mut indices: Vec<u32> = Vec::new();
        let mut values: Vec<f32> = Vec::new();
        for r in &outcome.rows {
            if let Ok(sparse) = r {
                for &(i, v) in sparse {
                    indices.push(i);
                    values.push(v);
                }
            }
            offsets.push(indices.len() as i32);
        }
        let struct_array = StructArray::new(
            struct_fields.clone(),
            vec![
                Arc::new(UInt32Array::from(indices)) as ArrayRef,
                Arc::new(Float32Array::from(values)) as ArrayRef,
            ],
            None,
        );
        let item = Arc::new(Field::new("item", DataType::Struct(struct_fields), false));
        let list = ListArray::try_new(
            item.clone(),
            OffsetBuffer::new(offsets.into()),
            Arc::new(struct_array),
            validity(outcome),
        )
        .map_err(|e| Status::internal(format!("Failed to build sparse column: {}", e)))?;
        let schema = Arc::new(Schema::new(vec![
            Field::new("sparse_embeddings", DataType::List(item), true),
            Field::new("error", DataType::Utf8, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![Arc::new(list) as ArrayRef, error_column(outcome)],
        )
        .map_err(|e| Status::internal(format!("Failed to create RecordBatch: {}", e)))
    }

    /// Serialize to Arrow IPC stream format with the requested compression.
    pub fn serialize(batch: &RecordBatch, compression: i32) -> Result<Vec<u8>, Status> {
        let compression = match mux::ArrowCompression::try_from(compression) {
            Ok(mux::ArrowCompression::Lz4) => Some(CompressionType::LZ4_FRAME),
            Ok(mux::ArrowCompression::None) => None,
            Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "unknown compression value {compression}"
                )));
            }
        };
        let options = IpcWriteOptions::default()
            .try_with_compression(compression)
            .map_err(|e| Status::internal(format!("Failed to set compression: {}", e)))?;
        let mut buffer = Vec::new();
        let mut writer = StreamWriter::try_new_with_options(&mut buffer, &batch.schema(), options)
            .map_err(|e| Status::internal(format!("Failed to create IPC writer: {}", e)))?;
        writer
            .write(batch)
            .map_err(|e| Status::internal(format!("Failed to write RecordBatch: {}", e)))?;
        writer
            .finish()
            .map_err(|e| Status::internal(format!("Failed to finish IPC writer: {}", e)))?;
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InstanceConfig;
    use crate::registry::Registry;
    use arrow::array::Array;
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
        TeiMultiplexerService::new(pool, 1024, 30)
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
        let service = TeiMultiplexerService::new(pool, 1024, 30);

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
                normalize: Some(false),
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
                normalize: Some(false),
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
        let service = TeiMultiplexerService::new(pool, 2048, 30);
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    // ========================================================================
    // Per-row error handling (embed_rows) Tests
    // ========================================================================

    /// Fake backend: `script[k]` is what the k-th opened stream yields,
    /// regardless of how many requests it received (mirrors tonic, where a
    /// mid-stream error can hide the OK responses buffered ahead of it).
    /// Each opened stream also records how many requests it was sent.
    type Script = Arc<std::sync::Mutex<Vec<Vec<Result<u32, Status>>>>>;
    type Sent = Arc<std::sync::Mutex<Vec<usize>>>;

    async fn run_rows(
        rows: Vec<Option<&str>>,
        script: Vec<Vec<Result<u32, Status>>>,
    ) -> (Result<RowOutcomes<u32>, Status>, Vec<usize>) {
        let script: Script = Arc::new(std::sync::Mutex::new(script));
        let sent: Sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sent_c = sent.clone();
        let result = embed_rows(
            rows.into_iter().map(|r| r.map(str::to_string)).collect(),
            |text: &str| text.to_string(),
            (),
            move |(), reqs: tokio_stream::Iter<std::vec::IntoIter<String>>| {
                let script = script.clone();
                let sent = sent_c.clone();
                Box::pin(async move {
                    let n = reqs.collect::<Vec<_>>().await.len();
                    sent.lock().unwrap().push(n);
                    let mut script = script.lock().unwrap();
                    assert!(
                        !script.is_empty(),
                        "backend opened more streams than scripted"
                    );
                    Ok(tokio_stream::iter(script.remove(0)))
                })
            },
        )
        .await;
        let sent = sent.lock().unwrap().clone();
        (result, sent)
    }

    fn bad(msg: &str) -> Result<u32, Status> {
        Err(Status::invalid_argument(msg))
    }

    #[tokio::test]
    async fn test_embed_rows_all_ok_single_stream() {
        let (res, sent) = run_rows(
            vec![Some("a"), Some("b"), Some("c")],
            vec![vec![Ok(1), Ok(2), Ok(3)]],
        )
        .await;
        assert_eq!(res.unwrap().rows, vec![Ok(1), Ok(2), Ok(3)]);
        assert_eq!(sent, vec![3]);
    }

    #[tokio::test]
    async fn test_embed_rows_probes_after_failure_with_buffered_ok_delivered() {
        // Stream 1 delivers row0 then fails (row1 bad). Probe row1 → fails →
        // recorded. Bulk resumes with rows 2..4.
        let (res, sent) = run_rows(
            vec![Some("a"), Some("b"), Some("c"), Some("d")],
            vec![
                vec![Ok(1), bad("too long")],
                vec![bad("too long")],
                vec![Ok(3), Ok(4)],
            ],
        )
        .await;
        let out = res.unwrap();
        assert_eq!(
            out.rows,
            vec![Ok(1), Err("too long".to_string()), Ok(3), Ok(4)]
        );
        assert_eq!(sent, vec![4, 1, 2]);
        assert_eq!(out.ok_count(), 3);
    }

    #[tokio::test]
    async fn test_embed_rows_probes_after_failure_with_buffered_ok_lost() {
        // Stream 1 fails immediately even though row0 was fine (tonic hid the
        // buffered OK). Probes continue past the OK row until the bad one is
        // found; bulk resumes at row2.
        let (res, sent) = run_rows(
            vec![Some("a"), Some("b"), Some("c")],
            vec![
                vec![bad("too long")],
                vec![Ok(1)],
                vec![bad("too long")],
                vec![Ok(3)],
            ],
        )
        .await;
        let out = res.unwrap();
        assert_eq!(out.rows, vec![Ok(1), Err("too long".to_string()), Ok(3)]);
        assert_eq!(sent, vec![3, 1, 1, 1]);
    }

    #[tokio::test]
    async fn test_embed_rows_consecutive_bad_rows() {
        let (res, sent) = run_rows(
            vec![Some("a"), Some("b"), Some("c")],
            vec![
                vec![bad("e1")], // bulk [a,b,c] fails
                vec![bad("e1")], // probe a → recorded
                vec![bad("e2")], // bulk [b,c] fails
                vec![bad("e2")], // probe b → recorded
                vec![Ok(3)],     // bulk [c]
            ],
        )
        .await;
        let out = res.unwrap();
        assert_eq!(
            out.rows,
            vec![Err("e1".to_string()), Err("e2".to_string()), Ok(3)]
        );
        assert_eq!(sent, vec![3, 1, 2, 1, 1]);
    }

    #[tokio::test]
    async fn test_embed_rows_last_row_bad() {
        let (res, sent) = run_rows(
            vec![Some("a"), Some("b")],
            vec![vec![Ok(1), bad("e")], vec![bad("e")]],
        )
        .await;
        assert_eq!(res.unwrap().rows, vec![Ok(1), Err("e".to_string())]);
        assert_eq!(sent, vec![2, 1]);
    }

    #[tokio::test]
    async fn test_embed_rows_null_inputs_never_sent() {
        let (res, sent) = run_rows(vec![None, Some("b"), None], vec![vec![Ok(2)]]).await;
        assert_eq!(
            res.unwrap().rows,
            vec![
                Err(NULL_INPUT_ERROR.to_string()),
                Ok(2),
                Err(NULL_INPUT_ERROR.to_string())
            ]
        );
        assert_eq!(sent, vec![1]);
    }

    #[tokio::test]
    async fn test_embed_rows_all_null_opens_no_stream() {
        let (res, sent) = run_rows(vec![None, None], vec![]).await;
        assert_eq!(res.unwrap().ok_count(), 0);
        assert!(sent.is_empty());
    }

    #[tokio::test]
    async fn test_embed_rows_backend_error_aborts_batch() {
        let (res, _) = run_rows(
            vec![Some("a"), Some("b"), Some("c")],
            vec![vec![Ok(1), Err(Status::unavailable("gpu gone"))]],
        )
        .await;
        let err = res.unwrap_err();
        assert_eq!(err.code(), Code::Unavailable);
        assert!(
            err.message().contains("after 1 of 3 rows"),
            "{}",
            err.message()
        );
        assert!(err.message().contains("gpu gone"));
    }

    #[tokio::test]
    async fn test_embed_rows_short_stream_is_internal_error() {
        let (res, _) = run_rows(vec![Some("a"), Some("b")], vec![vec![Ok(1)]]).await;
        let err = res.unwrap_err();
        assert_eq!(err.code(), Code::Internal);
        assert!(err.message().contains("expected 2 responses, got 1"));
    }

    #[tokio::test]
    async fn test_embed_rows_extra_responses_is_internal_error() {
        let (res, _) = run_rows(vec![Some("a")], vec![vec![Ok(1), Ok(2)]]).await;
        let err = res.unwrap_err();
        assert_eq!(err.code(), Code::Internal);
        assert!(err.message().contains("more than 1 responses"));
    }

    #[tokio::test]
    async fn test_embed_rows_open_failure_propagates_code() {
        let result: Result<RowOutcomes<u32>, Status> = embed_rows(
            vec![Some("a".to_string())],
            |t: &str| t.to_string(),
            (),
            |(), _reqs: tokio_stream::Iter<std::vec::IntoIter<String>>| {
                Box::pin(async {
                    Err::<tokio_stream::Iter<std::vec::IntoIter<Result<u32, Status>>>, _>(
                        Status::unavailable("connect refused"),
                    )
                })
            },
        )
        .await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::Unavailable);
        assert!(err.message().starts_with("backend stream failed"));
    }

    // ========================================================================
    // Arrow batch construction Tests
    // ========================================================================

    fn dense_outcome() -> RowOutcomes<Vec<f32>> {
        RowOutcomes {
            rows: vec![
                Ok(vec![1.0, 2.0]),
                Err("bad row".to_string()),
                Ok(vec![3.0, 4.0]),
            ],
        }
    }

    #[test]
    fn test_dense_batch_aligns_rows_and_errors() {
        use arrow::array::FixedSizeListArray;
        let batch = arrow_batch::dense_batch(&dense_outcome(), ArrowOutputDtype::F32).unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema().field(0).name(), "embeddings");
        assert_eq!(batch.schema().field(1).name(), "error");

        let emb = batch
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert_eq!(emb.value_length(), 2);
        assert!(emb.is_valid(0));
        assert!(emb.is_null(1));
        assert!(emb.is_valid(2));
        let row2 = emb.value(2);
        let row2 = row2.as_any().downcast_ref::<Float32Array>().unwrap();
        assert_eq!(row2.values(), &[3.0, 4.0]);

        let err = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(err.is_null(0));
        assert_eq!(err.value(1), "bad row");
        assert!(err.is_null(2));
    }

    #[test]
    fn test_dense_batch_all_ok_has_no_null_buffer() {
        let outcome = RowOutcomes {
            rows: vec![Ok(vec![1.0f32]), Ok(vec![2.0])],
        };
        let batch = arrow_batch::dense_batch(&outcome, ArrowOutputDtype::F32).unwrap();
        assert_eq!(batch.column(0).null_count(), 0);
        assert_eq!(batch.column(1).null_count(), 2);
    }

    #[test]
    fn test_dense_batch_all_failed_has_zero_dim() {
        use arrow::array::FixedSizeListArray;
        let outcome: RowOutcomes<Vec<f32>> = RowOutcomes {
            rows: vec![Err("x".to_string()), Err("y".to_string())],
        };
        let batch = arrow_batch::dense_batch(&outcome, ArrowOutputDtype::F32).unwrap();
        assert_eq!(batch.num_rows(), 2);
        let emb = batch
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert_eq!(emb.value_length(), 0);
        assert_eq!(emb.null_count(), 2);
    }

    #[test]
    fn test_dense_batch_f16_round_trips_values() {
        use arrow::array::{FixedSizeListArray, Float16Array};
        let batch = arrow_batch::dense_batch(&dense_outcome(), ArrowOutputDtype::F16).unwrap();
        let emb = batch
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert_eq!(emb.value_type(), DataType::Float16);
        assert!(emb.is_null(1));
        let row2 = emb.value(2);
        let row2 = row2.as_any().downcast_ref::<Float16Array>().unwrap();
        assert_eq!(row2.value(0).to_f32(), 3.0);
        assert_eq!(row2.value(1).to_f32(), 4.0);
        let f32_batch = arrow_batch::dense_batch(&dense_outcome(), ArrowOutputDtype::F32).unwrap();
        assert!(
            batch.column(0).get_array_memory_size() < f32_batch.column(0).get_array_memory_size()
        );
    }

    #[tokio::test]
    async fn test_output_dtype_resolution() {
        let service = create_test_service();
        assert_eq!(
            service
                .output_dtype(mux::OutputDtype::Unspecified as i32)
                .unwrap(),
            ArrowOutputDtype::F32
        );
        assert_eq!(
            service.output_dtype(mux::OutputDtype::F16 as i32).unwrap(),
            ArrowOutputDtype::F16
        );
        let service = service.with_default_output_dtype(ArrowOutputDtype::F16);
        assert_eq!(
            service
                .output_dtype(mux::OutputDtype::Unspecified as i32)
                .unwrap(),
            ArrowOutputDtype::F16
        );
        assert_eq!(
            service.output_dtype(mux::OutputDtype::F32 as i32).unwrap(),
            ArrowOutputDtype::F32
        );
        assert_eq!(
            service.output_dtype(9).unwrap_err().code(),
            Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn test_embed_arrow_noop_f16_output() {
        use arrow::array::FixedSizeListArray;
        let service = create_test_service();
        let arrow_ipc = ipc_from_array(Arc::new(StringArray::from(vec!["a", "b"])), "text");
        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            noop: true,
            output_dtype: mux::OutputDtype::F16 as i32,
            ..Default::default()
        });
        let response = service.embed_arrow(request).await.unwrap().into_inner();
        let mut reader = StreamReader::try_new(Cursor::new(response.arrow_ipc), None).unwrap();
        let batch = reader.next().unwrap().unwrap();
        let emb = batch
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert_eq!(emb.value_type(), DataType::Float16);
        assert_eq!(emb.value_length(), NOOP_EMBEDDING_DIM as i32);
    }

    #[test]
    fn test_dense_batch_rejects_mixed_dimensions() {
        let outcome = RowOutcomes {
            rows: vec![Ok(vec![1.0f32, 2.0]), Ok(vec![3.0])],
        };
        let err = arrow_batch::dense_batch(&outcome, ArrowOutputDtype::F32).unwrap_err();
        assert_eq!(err.code(), Code::Internal);
        assert!(err.message().contains("inconsistent embedding dimensions"));
    }

    #[test]
    fn test_sparse_batch_aligns_rows_and_errors() {
        let outcome: RowOutcomes<Vec<(u32, f32)>> = RowOutcomes {
            rows: vec![
                Ok(vec![(1, 0.5), (7, 0.25)]),
                Err("nope".to_string()),
                Ok(vec![]),
                Ok(vec![(3, 1.0)]),
            ],
        };
        let batch = arrow_batch::sparse_batch(&outcome).unwrap();
        assert_eq!(batch.num_rows(), 4);
        let list = batch
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        assert_eq!(list.value_offsets(), &[0, 2, 2, 2, 3]);
        assert!(list.is_null(1));
        assert!(list.is_valid(2)); // empty but present
        let err = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(err.value(1), "nope");
        assert_eq!(err.null_count(), 3);
    }

    fn ipc_from_array(array: ArrayRef, name: &str) -> Vec<u8> {
        use arrow::ipc::writer::StreamWriter;
        let schema = Arc::new(Schema::new(vec![Field::new(
            name,
            array.data_type().clone(),
            true,
        )]));
        let batch = RecordBatch::try_new(schema.clone(), vec![array]).unwrap();
        let mut buf = Vec::new();
        let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
        buf
    }

    #[test]
    fn test_parse_text_rows_preserves_nulls() {
        let ipc = ipc_from_array(
            Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])),
            "text",
        );
        let rows = arrow_batch::parse_text_rows(&ipc).unwrap();
        assert_eq!(
            rows,
            vec![Some("a".to_string()), None, Some("c".to_string())]
        );
    }

    #[test]
    fn test_parse_text_rows_accepts_large_and_view_utf8() {
        let ipc = ipc_from_array(
            Arc::new(arrow::array::LargeStringArray::from(vec!["x", "y"])),
            "text",
        );
        assert_eq!(arrow_batch::parse_text_rows(&ipc).unwrap().len(), 2);
        let ipc = ipc_from_array(
            Arc::new(arrow::array::StringViewArray::from(vec!["x"])),
            "text",
        );
        assert_eq!(arrow_batch::parse_text_rows(&ipc).unwrap().len(), 1);
    }

    #[test]
    fn test_parse_text_rows_rejects_no_columns() {
        use arrow::ipc::writer::StreamWriter;
        let schema = Arc::new(Schema::empty());
        let mut buf = Vec::new();
        let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
        let batch = RecordBatch::try_new_with_options(
            schema,
            vec![],
            &arrow::record_batch::RecordBatchOptions::new().with_row_count(Some(0)),
        )
        .unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
        let err = arrow_batch::parse_text_rows(&buf).unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("no columns"));
    }

    #[test]
    fn test_serialize_compression_modes() {
        let batch = arrow_batch::dense_batch(&dense_outcome(), ArrowOutputDtype::F32).unwrap();
        let plain = arrow_batch::serialize(&batch, mux::ArrowCompression::None as i32).unwrap();
        let lz4 = arrow_batch::serialize(&batch, mux::ArrowCompression::Lz4 as i32).unwrap();
        for buf in [&plain, &lz4] {
            let mut reader = StreamReader::try_new(Cursor::new(buf), None).unwrap();
            let rt = reader.next().unwrap().unwrap();
            assert_eq!(rt.num_rows(), 3);
            assert_eq!(rt.column(0).null_count(), 1);
        }
        let err = arrow_batch::serialize(&batch, 42).unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_embed_arrow_noop_null_row_reports_error() {
        let service = create_test_service();
        let arrow_ipc = ipc_from_array(Arc::new(StringArray::from(vec![Some("a"), None])), "text");
        let request = Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop: true,
            ..Default::default()
        });
        // noop mode short-circuits the backend entirely, so both rows succeed;
        // this pins that noop never touches the pool and keeps row count.
        let response = service.embed_arrow(request).await.unwrap().into_inner();
        let mut reader = StreamReader::try_new(Cursor::new(response.arrow_ipc), None).unwrap();
        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 2);
    }

    // ========================================================================
    // EmbedSparseArrow RPC Tests
    // ========================================================================

    #[tokio::test]
    async fn test_embed_sparse_arrow_missing_target() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedSparseArrowRequest {
            target: None,
            arrow_ipc: vec![],
            truncate: true,
            noop: false,
            ..Default::default()
        });
        let result = service.embed_sparse_arrow(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_embed_sparse_arrow_invalid_ipc() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedSparseArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc: vec![1, 2, 3, 4], // Invalid Arrow IPC bytes
            truncate: true,
            noop: false,
            ..Default::default()
        });
        let result = service.embed_sparse_arrow(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Invalid Arrow IPC"));
    }

    #[tokio::test]
    async fn test_embed_sparse_arrow_empty_ipc() {
        let service = create_test_service();
        let request = Request::new(mux::EmbedSparseArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc: vec![], // Empty Arrow IPC
            truncate: true,
            noop: false,
            ..Default::default()
        });
        let result = service.embed_sparse_arrow(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_embed_sparse_arrow_noop_mode() {
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

        let request = Request::new(mux::EmbedSparseArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            noop: true, // Noop mode - returns dummy sparse embeddings
            ..Default::default()
        });

        let result = service.embed_sparse_arrow(request).await;
        assert!(result.is_ok());

        // Verify response has sparse embeddings
        let response = result.unwrap().into_inner();
        assert!(!response.arrow_ipc.is_empty());

        // Decode and verify
        let cursor = std::io::Cursor::new(response.arrow_ipc);
        let mut reader = StreamReader::try_new(cursor, None).unwrap();
        let result_batch = reader.next().unwrap().unwrap();
        assert_eq!(result_batch.num_rows(), 2); // 2 texts -> 2 sparse embeddings
    }

    #[tokio::test]
    async fn test_embed_sparse_arrow_wrong_column_type() {
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

        let request = Request::new(mux::EmbedSparseArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            noop: true,
            ..Default::default()
        });

        let result = service.embed_sparse_arrow(request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("StringArray"));
    }

    #[tokio::test]
    async fn test_embed_sparse_arrow_instance_not_found() {
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

        let request = Request::new(mux::EmbedSparseArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName(
                    "nonexistent".to_string(),
                )),
            }),
            arrow_ipc,
            truncate: true,
            noop: false, // Not noop, so it will try to find instance
            ..Default::default()
        });

        let result = service.embed_sparse_arrow(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), Code::NotFound);
    }

    #[tokio::test]
    async fn test_embed_sparse_arrow_noop_empty_batch() {
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

        let request = Request::new(mux::EmbedSparseArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            noop: true,
            ..Default::default()
        });

        let result = service.embed_sparse_arrow(request).await;
        assert!(result.is_ok());

        // Verify empty response
        let response = result.unwrap().into_inner();
        let cursor = std::io::Cursor::new(response.arrow_ipc);
        let mut reader = StreamReader::try_new(cursor, None).unwrap();
        let result_batch = reader.next().unwrap().unwrap();
        assert_eq!(result_batch.num_rows(), 0);
    }

    #[tokio::test]
    async fn test_embed_sparse_arrow_noop_verify_structure() {
        use arrow::array::{ListArray, StringArray, StructArray};
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

        let request = Request::new(mux::EmbedSparseArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc,
            truncate: true,
            noop: true,
            ..Default::default()
        });

        let result = service.embed_sparse_arrow(request).await;
        assert!(result.is_ok());

        // Verify sparse embedding structure
        let response = result.unwrap().into_inner();
        let cursor = std::io::Cursor::new(response.arrow_ipc);
        let mut reader = StreamReader::try_new(cursor, None).unwrap();
        let result_batch = reader.next().unwrap().unwrap();

        // Get sparse_embeddings column and verify it's a ListArray
        let sparse_col = result_batch
            .column(0)
            .as_any()
            .downcast_ref::<ListArray>()
            .expect("Should be ListArray");

        assert_eq!(sparse_col.len(), 1); // 1 row

        // Get the struct values
        let struct_values = sparse_col
            .values()
            .as_any()
            .downcast_ref::<StructArray>()
            .expect("Should be StructArray");

        // Verify struct has index and value fields
        assert_eq!(struct_values.num_columns(), 2);

        // Noop mode returns 3 values per row
        let first_row_len = sparse_col.value_length(0);
        assert_eq!(first_row_len, 3);

        // Verify index and value arrays exist and have correct types
        let indices = struct_values
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("Index should be UInt32Array");
        let values = struct_values
            .column(1)
            .as_any()
            .downcast_ref::<Float32Array>()
            .expect("Value should be Float32Array");

        assert_eq!(indices.len(), 3);
        assert_eq!(values.len(), 3);

        // Verify noop values: [(0, 1.0), (100, 0.5), (200, 0.25)]
        assert_eq!(indices.value(0), 0);
        assert_eq!(values.value(0), 1.0);
        assert_eq!(indices.value(1), 100);
        assert_eq!(values.value(1), 0.5);
        assert_eq!(indices.value(2), 200);
        assert_eq!(values.value(2), 0.25);
    }

    // ========================================================================
    // Request Timeout Tests
    // ========================================================================

    #[tokio::test]
    async fn test_timeout_configuration_enabled() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);
        let service = TeiMultiplexerService::new(pool, 1024, 30);
        assert!(service.request_timeout.is_some());
        assert_eq!(service.request_timeout.unwrap(), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn test_timeout_configuration_disabled() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);
        let service = TeiMultiplexerService::new(pool, 1024, 0);
        assert!(service.request_timeout.is_none());
    }

    #[tokio::test]
    async fn test_timeout_configuration_various_values() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        for timeout_secs in [1, 5, 10, 60, 300] {
            let pool = BackendPool::new(registry.clone());
            let service = TeiMultiplexerService::new(pool, 1024, timeout_secs);
            assert_eq!(
                service.request_timeout.unwrap(),
                Duration::from_secs(timeout_secs)
            );
        }
    }

    #[tokio::test]
    async fn test_with_timeout_wrapper_success() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);
        let service = TeiMultiplexerService::new(pool, 1024, 30);

        // Simulate a fast operation that completes within timeout
        let result = service
            .with_timeout(async { Ok::<_, Status>("success") })
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_with_timeout_wrapper_no_timeout_configured() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);
        let service = TeiMultiplexerService::new(pool, 1024, 0);

        // With no timeout, operations should complete without deadline
        let result = service
            .with_timeout(async { Ok::<_, Status>("success") })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_with_timeout_wrapper_timeout_exceeded() {
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        let pool = BackendPool::new(registry);
        // Very short timeout for testing
        let service = TeiMultiplexerService::new(pool, 1024, 1);

        // Simulate a slow operation that exceeds timeout
        let result: Result<(), Status> = service
            .with_timeout(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(())
            })
            .await;

        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), Code::DeadlineExceeded);
        assert!(status.message().contains("timeout"));
    }
}
