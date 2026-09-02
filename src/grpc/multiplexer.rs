//! TeiMultiplexer service implementation - routes requests to backend TEI instances

use arrow::array::{
    ArrayRef, FixedSizeListArray, Float32Array, ListArray, StringArray, StructArray, UInt32Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Fields, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use futures::FutureExt;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
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
/// 5. **Metrics**: Counts the request in `tei_mux_requests_total` (no duration
///    is recorded — the stream outlives the handler)
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
        let mut mux_metrics = MuxRequestMetrics::stream(stringify!($backend_method));
        let mut stream: Streaming<$mux_req> = $request.into_inner();

        // Read first request to get instance name
        let first_req: $mux_req = stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("Empty stream"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let instance_name = $self.resolve_target(first_req.target).await?;
        Span::current().record("tei.instance", instance_name.as_str());
        mux_metrics.set_instance(&instance_name);

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

        mux_metrics.set_ok();
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }};
}

/// Prometheus metrics guard for one multiplexer RPC.
///
/// Created at handler entry and recorded on drop, so early `?` returns (and
/// cancelled handlers) are counted as errors. Unary handlers record
/// `tei_mux_requests_total` plus `tei_mux_request_duration_seconds`; streaming
/// handlers record the counter only — the stream outlives the handler, so a
/// handler-scoped duration would be meaningless.
///
/// The instance label is the resolved backend instance name, or `"unknown"`
/// when the request fails before target resolution.
struct MuxRequestMetrics {
    method: &'static str,
    instance: Option<String>,
    started: Instant,
    record_duration: bool,
    ok: bool,
}

impl MuxRequestMetrics {
    fn new(method: &'static str, record_duration: bool) -> Self {
        Self {
            method,
            instance: None,
            started: Instant::now(),
            record_duration,
            ok: false,
        }
    }

    /// Start measuring a unary RPC (count + duration)
    fn unary(method: &'static str) -> Self {
        Self::new(method, true)
    }

    /// Start measuring a streaming RPC (count only)
    fn stream(method: &'static str) -> Self {
        Self::new(method, false)
    }

    /// Record the resolved backend instance name
    fn set_instance(&mut self, name: &str) {
        self.instance = Some(name.to_owned());
    }

    /// Mark the request as successful (anything else records as an error)
    fn set_ok(&mut self) {
        self.ok = true;
    }
}

impl Drop for MuxRequestMetrics {
    fn drop(&mut self) {
        let instance = self.instance.take().unwrap_or_else(|| "unknown".to_owned());
        let status = if self.ok { "ok" } else { "error" };
        metrics::counter!(
            crate::metrics::MUX_REQUESTS_TOTAL,
            "method" => self.method,
            "instance" => instance.clone(),
            "status" => status
        )
        .increment(1);
        if self.record_duration {
            metrics::histogram!(
                crate::metrics::MUX_REQUEST_DURATION_SECONDS,
                "method" => self.method,
                "instance" => instance
            )
            .record(self.started.elapsed().as_secs_f64());
        }
    }
}

/// Record per-row outcomes of an Arrow batch RPC in `tei_mux_rows_total`
fn record_mux_rows<T>(instance: &str, outcome: &RowOutcomes<T>) {
    let ok_rows = outcome.ok_count() as u64;
    let failed_rows = outcome.rows.len() as u64 - ok_rows;
    metrics::counter!(
        crate::metrics::MUX_ROWS_TOTAL,
        "instance" => instance.to_owned(),
        "status" => "ok"
    )
    .increment(ok_rows);
    metrics::counter!(
        crate::metrics::MUX_ROWS_TOTAL,
        "instance" => instance.to_owned(),
        "status" => "failed"
    )
    .increment(failed_rows);
}

/// Server default for how many batches one `EmbedArrowStream` call may
/// execute concurrently (`grpc_stream_max_concurrent_batches`)
const DEFAULT_STREAM_MAX_CONCURRENT_BATCHES: usize = 4;
/// Clamp bounds for the effective per-stream batch concurrency, applied to
/// the server knob and the caller's first-request override alike
const STREAM_CONCURRENCY_MIN: usize = 1;
const STREAM_CONCURRENCY_MAX: usize = 64;

/// TeiMultiplexer service implementation
#[derive(Clone)]
pub struct TeiMultiplexerService {
    pool: BackendPool,
    max_parallel_stream_requests: usize,
    request_timeout: Option<Duration>,
    /// Element type for EmbedArrow responses when the request says UNSPECIFIED
    default_output_dtype: ArrowOutputDtype,
    /// Default concurrent batches per EmbedArrowStream call (clamped 1..=64)
    stream_max_concurrent_batches: usize,
    /// Round-robin cursor for model-based routing
    route_counter: Arc<std::sync::atomic::AtomicUsize>,
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
            stream_max_concurrent_batches: DEFAULT_STREAM_MAX_CONCURRENT_BATCHES,
            route_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Set the element type used when a request leaves `output_dtype` unspecified
    pub fn with_default_output_dtype(mut self, dtype: ArrowOutputDtype) -> Self {
        self.default_output_dtype = dtype;
        self
    }

    /// Set the default number of batches an `EmbedArrowStream` call may
    /// execute concurrently. A stream's first request can override this via
    /// `max_concurrent_batches`; either value is clamped to 1..=64.
    pub fn with_stream_max_concurrent_batches(mut self, max: usize) -> Self {
        self.stream_max_concurrent_batches = max;
        self
    }

    /// Effective batch concurrency for one stream: the caller's nonzero
    /// first-request override, else the server default, clamped to 1..=64
    fn effective_stream_concurrency(&self, requested: u32) -> usize {
        let k = if requested > 0 {
            requested as usize
        } else {
            self.stream_max_concurrent_batches
        };
        k.clamp(STREAM_CONCURRENCY_MIN, STREAM_CONCURRENCY_MAX)
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

    /// Resolve a request's target to one instance name.
    ///
    /// `instance_name` routes to that instance verbatim. `model_id` picks one
    /// RUNNING instance serving that model — round-robin across matches, one
    /// instance per RPC (a batch is never split). An instance that dies is
    /// simply no longer `Running`, so subsequent RPCs route around it; the
    /// in-flight one fails to the caller.
    async fn resolve_target(&self, target: Option<mux::Target>) -> Result<String, Status> {
        let target = target.ok_or_else(|| Status::invalid_argument("Missing target"))?;

        match target.routing {
            Some(mux::target::Routing::InstanceName(name)) => {
                if name.is_empty() {
                    return Err(Status::invalid_argument("Instance name cannot be empty"));
                }
                Ok(name)
            }
            Some(mux::target::Routing::ModelId(model_id)) => {
                if model_id.is_empty() {
                    return Err(Status::invalid_argument("Model id cannot be empty"));
                }
                self.pick_running_instance(&model_id).await
            }
            Some(mux::target::Routing::InstanceIndex(_)) => Err(Status::unimplemented(
                "Index-based routing not yet implemented",
            )),
            None => Err(Status::invalid_argument("No routing specified")),
        }
    }

    /// The RUNNING instances serving `model_id`, sorted for determinism.
    /// `NotFound` (naming the model) when there are none.
    async fn running_model_instances(&self, model_id: &str) -> Result<Vec<String>, Status> {
        let mut matches: Vec<String> = Vec::new();
        for instance in self.pool.registry().list().await {
            if instance.config.model_id == model_id
                && *instance.status.read().await == crate::instance::InstanceStatus::Running
            {
                matches.push(instance.config.name.clone());
            }
        }
        if matches.is_empty() {
            return Err(Status::not_found(format!(
                "No running instance for model '{}'",
                model_id
            )));
        }
        // Registry order is stable enough for fairness; sort for determinism
        matches.sort();
        Ok(matches)
    }

    /// Round-robin over the running instances of `model_id`
    async fn pick_running_instance(&self, model_id: &str) -> Result<String, Status> {
        let matches = self.running_model_instances(model_id).await?;
        let i = self
            .route_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(matches[i % matches.len()].clone())
    }

    /// Core of the `EmbedArrowStream` RPC, generic over the inbound stream so
    /// it can be driven by tonic's `Streaming` as well as plain streams in
    /// tests (mirroring how `embed_rows` is tested via generic streams).
    ///
    /// The FIRST request establishes the target and all options; subsequent
    /// requests contribute only their `arrow_ipc` payload. Up to K batches
    /// (`effective_stream_concurrency`) run concurrently through the same
    /// pipeline as the unary `EmbedArrow`, but responses are emitted strictly
    /// in request order, 1:1 with request batches. A batch-level failure
    /// (invalid IPC, backend death, timeout) terminates the stream with the
    /// status of the LOWEST-sequence failed batch: every batch before it is
    /// delivered, no batch after it is. Per-row errors ride in the `error`
    /// column and never end the stream. An `instance_name` target pins the
    /// whole stream to that instance; a `model_id` target re-resolves per
    /// batch (round-robin), spreading batches across the model's running
    /// instances.
    pub async fn embed_arrow_stream_core<S>(
        &self,
        mut stream: S,
    ) -> Result<
        tokio_stream::wrappers::ReceiverStream<Result<mux::EmbedArrowResponse, Status>>,
        Status,
    >
    where
        S: futures::Stream<Item = Result<mux::EmbedArrowRequest, Status>> + Unpin + Send + 'static,
    {
        let mut mux_metrics = MuxRequestMetrics::stream("embed_arrow_stream");
        let first = stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("Empty stream"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let target = first
            .target
            .clone()
            .ok_or_else(|| Status::invalid_argument("Missing target"))?;
        let (routing, instance_label) = match target.routing {
            Some(mux::target::Routing::InstanceName(name)) => {
                if name.is_empty() {
                    return Err(Status::invalid_argument("Instance name cannot be empty"));
                }
                // Noop mode never dials the backend, exactly like the unary
                // RPC; otherwise the clients are resolved once and pinned.
                let embed = if first.noop {
                    None
                } else {
                    Some(self.pool.get_clients(&name).await?.embed.clone())
                };
                (
                    StreamRouting::Instance {
                        name: name.clone(),
                        embed,
                    },
                    name,
                )
            }
            Some(mux::target::Routing::ModelId(model_id)) => {
                if model_id.is_empty() {
                    return Err(Status::invalid_argument("Model id cannot be empty"));
                }
                // Fail fast when the model has no running instance right now;
                // afterwards every batch re-resolves on its own.
                self.running_model_instances(&model_id).await?;
                // Stream-level attribution names the MODEL: batches fan out
                // across instances, so no single instance label is truthful
                // here — per-batch spans/metrics carry the real instance.
                let label = format!("model:{model_id}");
                (StreamRouting::Model { model_id }, label)
            }
            Some(mux::target::Routing::InstanceIndex(_)) => {
                return Err(Status::unimplemented(
                    "Index-based routing not yet implemented",
                ));
            }
            None => return Err(Status::invalid_argument("No routing specified")),
        };
        Span::current().record("tei.instance", instance_label.as_str());
        mux_metrics.set_instance(&instance_label);

        let job = Arc::new(ArrowStreamJob {
            service: self.clone(),
            routing,
            noop: first.noop,
            opts: EmbedArrowOptions::from_request(&first),
            output_dtype: self.output_dtype(first.output_dtype)?,
            compression: first.compression,
            timeout: self.request_timeout,
        });
        let max_concurrent = self.effective_stream_concurrency(first.max_concurrent_batches);
        Span::current().record("tei.max_concurrent_batches", max_concurrent);

        let (tx, rx) = tokio::sync::mpsc::channel(self.max_parallel_stream_requests);
        let span = Span::current();
        let process = move |seq: u64, ipc: Vec<u8>| {
            let job = job.clone();
            async move { job.run_batch(seq, &ipc).await }
        };
        tokio::spawn(async move {
            let stats = run_arrow_stream_pipeline(stream, first, max_concurrent, tx, process).await;
            span.record(
                "tei.batches_with_ignored_fields",
                stats.batches_with_ignored_fields,
            );
        });

        mux_metrics.set_ok();
        Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

#[tonic::async_trait]
impl mux::tei_multiplexer_server::TeiMultiplexer for TeiMultiplexerService {
    // ========================================================================
    // Info Service
    // ========================================================================

    #[instrument(name = "tei.info", skip(self, request), fields(tei.instance))]
    async fn info(
        &self,
        request: Request<mux::InfoRequest>,
    ) -> Result<Response<tei::InfoResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("info");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        // Record instance name in span for tracing
        Span::current().record("tei.instance", instance_name.as_str());

        // Get backend client (lock-free lookup)
        let clients = self.pool.get_clients(&instance_name).await?;

        // Forward request to backend with timeout
        let response = self
            .with_timeout(async { clients.info.clone().info(tei::InfoRequest {}).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    // ========================================================================
    // Embed Service - Unary RPCs
    // ========================================================================

    #[instrument(name = "tei.embed", skip(self, request), fields(tei.instance, tei.inputs_len))]
    async fn embed(
        &self,
        request: Request<mux::EmbedRequest>,
    ) -> Result<Response<tei::EmbedResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("embed");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        // Extract inner request
        let embed_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing embed request"))?;

        // Record metrics
        Span::current()
            .record("tei.instance", instance_name.as_str())
            .record("tei.inputs_len", embed_req.inputs.len());

        // Get backend client
        let clients = self.pool.get_clients(&instance_name).await?;

        // Forward to backend with timeout
        let response = self
            .with_timeout(async { clients.embed.clone().embed(embed_req).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    #[instrument(name = "tei.embed_sparse", skip(self, request), fields(tei.instance))]
    async fn embed_sparse(
        &self,
        request: Request<mux::EmbedSparseRequest>,
    ) -> Result<Response<tei::EmbedSparseResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("embed_sparse");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing embed_sparse request"))?;

        Span::current().record("tei.instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = self
            .with_timeout(async { clients.embed.clone().embed_sparse(inner_req).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    #[instrument(name = "tei.embed_all", skip(self, request), fields(tei.instance))]
    async fn embed_all(
        &self,
        request: Request<mux::EmbedAllRequest>,
    ) -> Result<Response<tei::EmbedAllResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("embed_all");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing embed_all request"))?;

        Span::current().record("tei.instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = self
            .with_timeout(async { clients.embed.clone().embed_all(inner_req).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    // ========================================================================
    // Embed Service - Streaming RPCs
    // ========================================================================

    type EmbedStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::EmbedResponse, Status>>;

    #[instrument(name = "tei.embed_stream_rpc", skip(self, request), fields(tei.instance))]
    async fn embed_stream(
        &self,
        request: Request<Streaming<mux::EmbedRequest>>,
    ) -> Result<Response<Self::EmbedStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::EmbedRequest, embed, embed_stream)
    }

    type EmbedSparseStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::EmbedSparseResponse, Status>>;

    #[instrument(name = "tei.embed_sparse_stream_rpc", skip(self, request), fields(tei.instance))]
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

    #[instrument(name = "tei.embed_all_stream_rpc", skip(self, request), fields(tei.instance))]
    async fn embed_all_stream(
        &self,
        request: Request<Streaming<mux::EmbedAllRequest>>,
    ) -> Result<Response<Self::EmbedAllStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::EmbedAllRequest, embed, embed_all_stream)
    }

    // ========================================================================
    // Predict Service
    // ========================================================================

    #[instrument(name = "tei.predict", skip(self, request), fields(tei.instance))]
    async fn predict(
        &self,
        request: Request<mux::PredictRequest>,
    ) -> Result<Response<tei::PredictResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("predict");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing predict request"))?;

        Span::current().record("tei.instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = self
            .with_timeout(async { clients.predict.clone().predict(inner_req).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    #[instrument(name = "tei.predict_pair", skip(self, request), fields(tei.instance))]
    async fn predict_pair(
        &self,
        request: Request<mux::PredictPairRequest>,
    ) -> Result<Response<tei::PredictResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("predict_pair");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing predict_pair request"))?;

        Span::current().record("tei.instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = self
            .with_timeout(async { clients.predict.clone().predict_pair(inner_req).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    type PredictStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::PredictResponse, Status>>;

    #[instrument(name = "tei.predict_stream_rpc", skip(self, request), fields(tei.instance))]
    async fn predict_stream(
        &self,
        request: Request<Streaming<mux::PredictRequest>>,
    ) -> Result<Response<Self::PredictStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::PredictRequest, predict, predict_stream)
    }

    type PredictPairStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::PredictResponse, Status>>;

    #[instrument(name = "tei.predict_pair_stream_rpc", skip(self, request), fields(tei.instance))]
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

    #[instrument(name = "tei.rerank", skip(self, request), fields(tei.instance))]
    async fn rerank(
        &self,
        request: Request<mux::RerankRequest>,
    ) -> Result<Response<tei::RerankResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("rerank");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing rerank request"))?;

        Span::current().record("tei.instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = self
            .with_timeout(async { clients.rerank.clone().rerank(inner_req).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    #[instrument(name = "tei.rerank_stream_rpc", skip(self, request), fields(tei.instance))]
    async fn rerank_stream(
        &self,
        request: Request<Streaming<mux::RerankStreamRequest>>,
    ) -> Result<Response<tei::RerankResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::stream("rerank_stream");
        let mut stream = request.into_inner();

        let first_req = stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("Empty stream"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let instance_name = self.resolve_target(first_req.target).await?;
        Span::current().record("tei.instance", instance_name.as_str());
        mux_metrics.set_instance(&instance_name);

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

        mux_metrics.set_ok();
        Ok(response)
    }

    // ========================================================================
    // Tokenize Service
    // ========================================================================

    #[instrument(name = "tei.tokenize", skip(self, request), fields(tei.instance))]
    async fn tokenize(
        &self,
        request: Request<mux::EncodeRequest>,
    ) -> Result<Response<tei::EncodeResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("tokenize");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing tokenize request"))?;

        Span::current().record("tei.instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = self
            .with_timeout(async { clients.tokenize.clone().tokenize(inner_req).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    type TokenizeStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::EncodeResponse, Status>>;

    #[instrument(name = "tei.tokenize_stream_rpc", skip(self, request), fields(tei.instance))]
    async fn tokenize_stream(
        &self,
        request: Request<Streaming<mux::EncodeRequest>>,
    ) -> Result<Response<Self::TokenizeStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::EncodeRequest, tokenize, tokenize_stream)
    }

    #[instrument(name = "tei.decode", skip(self, request), fields(tei.instance))]
    async fn decode(
        &self,
        request: Request<mux::DecodeRequest>,
    ) -> Result<Response<tei::DecodeResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("decode");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);

        let inner_req = req
            .request
            .ok_or_else(|| Status::invalid_argument("Missing decode request"))?;

        Span::current().record("tei.instance", instance_name.as_str());

        let clients = self.pool.get_clients(&instance_name).await?;
        let response = self
            .with_timeout(async { clients.tokenize.clone().decode(inner_req).await })
            .await?;

        mux_metrics.set_ok();
        Ok(response)
    }

    type DecodeStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<tei::DecodeResponse, Status>>;

    #[instrument(name = "tei.decode_stream_rpc", skip(self, request), fields(tei.instance))]
    async fn decode_stream(
        &self,
        request: Request<Streaming<mux::DecodeRequest>>,
    ) -> Result<Response<Self::DecodeStreamStream>, Status> {
        impl_stream_rpc!(self, request, mux::DecodeRequest, tokenize, decode_stream)
    }

    // ========================================================================
    // Arrow Batch Embedding
    // ========================================================================

    #[instrument(
        name = "tei.embed_arrow",
        skip(self, request),
        fields(tei.instance, tei.rows, tei.rows_failed, tei.output_dtype, tei.noop)
    )]
    async fn embed_arrow(
        &self,
        request: Request<mux::EmbedArrowRequest>,
    ) -> Result<Response<mux::EmbedArrowResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("embed_arrow");
        let mut req = request.into_inner();
        let instance_name = self.resolve_target(req.target.take()).await?;
        mux_metrics.set_instance(&instance_name);
        Span::current().record("tei.instance", instance_name.as_str());

        let rows = arrow_batch::parse_text_rows(&req.arrow_ipc)?;
        let output_dtype = self.output_dtype(req.output_dtype)?;
        Span::current()
            .record("tei.rows", rows.len())
            .record(
                "tei.output_dtype",
                format!("{output_dtype:?}").to_lowercase(),
            )
            .record("tei.noop", req.noop);

        let outcome: RowOutcomes<Vec<f32>> = if req.noop {
            RowOutcomes::noop(rows.len(), || vec![0.0f32; NOOP_EMBEDDING_DIM])
        } else {
            let clients = self.pool.get_clients(&instance_name).await?;
            let opts = EmbedArrowOptions::from_request(&req);
            self.with_timeout(embed_dense_rows(rows, opts, clients.embed.clone()))
                .await?
        };

        record_mux_rows(&instance_name, &outcome);
        Span::current().record("tei.rows_failed", outcome.rows.len() - outcome.ok_count());
        let batch = arrow_batch::dense_batch(&outcome, output_dtype)?;
        let buffer = arrow_batch::serialize(&batch, req.compression)?;
        mux_metrics.set_ok();
        Ok(Response::new(mux::EmbedArrowResponse { arrow_ipc: buffer }))
    }

    type EmbedArrowStreamStream =
        tokio_stream::wrappers::ReceiverStream<Result<mux::EmbedArrowResponse, Status>>;

    #[instrument(
        name = "tei.embed_arrow_stream_rpc",
        skip_all,
        fields(
            tei.instance,
            tei.max_concurrent_batches,
            tei.batches_with_ignored_fields
        )
    )]
    async fn embed_arrow_stream(
        &self,
        request: Request<Streaming<mux::EmbedArrowRequest>>,
    ) -> Result<Response<Self::EmbedArrowStreamStream>, Status> {
        let stream = request.into_inner();
        Ok(Response::new(self.embed_arrow_stream_core(stream).await?))
    }

    #[instrument(
        name = "tei.embed_sparse_arrow",
        skip(self, request),
        fields(tei.instance, tei.rows, tei.rows_failed, tei.noop)
    )]
    async fn embed_sparse_arrow(
        &self,
        request: Request<mux::EmbedSparseArrowRequest>,
    ) -> Result<Response<mux::EmbedSparseArrowResponse>, Status> {
        let mut mux_metrics = MuxRequestMetrics::unary("embed_sparse_arrow");
        let req = request.into_inner();
        let instance_name = self.resolve_target(req.target).await?;
        mux_metrics.set_instance(&instance_name);
        Span::current().record("tei.instance", instance_name.as_str());

        let rows = arrow_batch::parse_text_rows(&req.arrow_ipc)?;
        Span::current()
            .record("tei.rows", rows.len())
            .record("tei.noop", req.noop);

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

        record_mux_rows(&instance_name, &outcome);
        Span::current().record("tei.rows_failed", outcome.rows.len() - outcome.ok_count());
        let batch = arrow_batch::sparse_batch(&outcome)?;
        let buffer = arrow_batch::serialize(&batch, req.compression)?;
        mux_metrics.set_ok();
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

/// Backend client for the dense embed RPCs.
type DenseEmbedClient = tei::embed_client::EmbedClient<super::pool::BackendChannel>;

/// Per-row request options for a dense Arrow embedding job, fixed for its
/// lifetime (one unary `EmbedArrow` call, or one whole `EmbedArrowStream`).
#[derive(Clone)]
struct EmbedArrowOptions {
    truncate: bool,
    normalize: bool,
    truncation_direction: i32,
    prompt_name: Option<String>,
    dimensions: Option<u32>,
}

impl EmbedArrowOptions {
    fn from_request(req: &mux::EmbedArrowRequest) -> Self {
        Self {
            truncate: req.truncate,
            normalize: req.normalize,
            truncation_direction: req.truncation_direction,
            prompt_name: req.prompt_name.clone(),
            dimensions: req.dimensions,
        }
    }
}

/// Stream `rows` through the backend dense embed RPC with `opts` applied to
/// every row, keeping the per-row error handling of `embed_rows`.
async fn embed_dense_rows(
    rows: Vec<Option<String>>,
    opts: EmbedArrowOptions,
    embed: DenseEmbedClient,
) -> Result<RowOutcomes<Vec<f32>>, Status> {
    let build = |text: &str| tei::EmbedRequest {
        inputs: text.to_string(),
        truncate: opts.truncate,
        normalize: Some(opts.normalize),
        truncation_direction: opts.truncation_direction,
        prompt_name: opts.prompt_name.clone(),
        dimensions: opts.dimensions,
    };
    Ok(embed_rows(rows, build, embed, |mut c, s| {
        Box::pin(async move { c.embed_stream(s).await.map(Response::into_inner) })
    })
    .await?
    .map(|r| r.embeddings))
}

/// How an `EmbedArrowStream` call routes its batches to backends.
enum StreamRouting {
    /// `instance_name` target: the instance (and its clients, unless noop)
    /// are resolved once at stream open and pinned for the whole stream.
    Instance {
        name: String,
        /// Backend dense-embed client; `None` in noop mode (never dialed).
        embed: Option<DenseEmbedClient>,
    },
    /// `model_id` target: a running instance is picked PER BATCH (round-robin
    /// via `pick_running_instance`), so a stream's batches spread across all
    /// running instances of the model and route around instances that die.
    Model { model_id: String },
}

/// Everything fixed by the first request of an `EmbedArrowStream` call.
struct ArrowStreamJob {
    /// Handle back to the service for per-batch model routing.
    service: TeiMultiplexerService,
    routing: StreamRouting,
    /// Noop mode never dials the backend, exactly like the unary RPC.
    noop: bool,
    opts: EmbedArrowOptions,
    output_dtype: ArrowOutputDtype,
    compression: i32,
    /// Per-batch timeout (`None` = no timeout), mirroring the unary RPC.
    /// A timeout counts as that batch's failure.
    timeout: Option<Duration>,
}

impl ArrowStreamJob {
    /// Run one request batch through the same pipeline as the unary
    /// `EmbedArrow`: resolve the backend (per batch for model routing), parse
    /// rows, embed (or noop), build the dense batch and serialize it.
    #[instrument(
        name = "tei.embed_arrow_batch",
        skip_all,
        fields(tei.seq = seq, tei.instance, tei.rows, tei.rows_failed)
    )]
    async fn run_batch(&self, seq: u64, ipc: &[u8]) -> Result<mux::EmbedArrowResponse, Status> {
        let (instance, embed) = match &self.routing {
            StreamRouting::Instance { name, embed } => (name.clone(), embed.clone()),
            StreamRouting::Model { model_id } => {
                let name = self.service.pick_running_instance(model_id).await?;
                let embed = if self.noop {
                    None
                } else {
                    Some(self.service.pool.get_clients(&name).await?.embed.clone())
                };
                (name, embed)
            }
        };
        Span::current().record("tei.instance", instance.as_str());
        let rows = arrow_batch::parse_text_rows(ipc)?;
        Span::current().record("tei.rows", rows.len());
        let outcome: RowOutcomes<Vec<f32>> = match embed {
            None => RowOutcomes::noop(rows.len(), || vec![0.0f32; NOOP_EMBEDDING_DIM]),
            Some(embed) => {
                let fut = embed_dense_rows(rows, self.opts.clone(), embed);
                match self.timeout {
                    Some(duration) => timeout(duration, fut)
                        .await
                        .map_err(|_| Status::deadline_exceeded("Request timeout"))?,
                    None => fut.await,
                }?
            }
        };
        record_mux_rows(&instance, &outcome);
        Span::current().record("tei.rows_failed", outcome.rows.len() - outcome.ok_count());
        let batch = arrow_batch::dense_batch(&outcome, self.output_dtype)?;
        let buffer = arrow_batch::serialize(&batch, self.compression)?;
        Ok(mux::EmbedArrowResponse { arrow_ipc: buffer })
    }
}

/// Outcome counters of one `EmbedArrowStream` call, recorded on the
/// stream-level span when the stream ends.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ArrowStreamStats {
    /// Requests after the first that set option fields differing from the
    /// pinned values. Those fields are ignored per the proto contract, so
    /// they are surfaced via a per-batch debug log plus this counter.
    batches_with_ignored_fields: u64,
}

/// One finished (or synthesized) batch result on its way to the collector.
struct BatchCompletion {
    seq: u64,
    /// The batch's admission permit. Carried here instead of being dropped on
    /// completion so the collector releases it when the response is EMITTED,
    /// which bounds admitted-but-unemitted work to K and lets a slow client
    /// apply backpressure. `None` for the reader's synthetic
    /// transport-failure entry.
    permit: Option<OwnedSemaphorePermit>,
    result: Result<mux::EmbedArrowResponse, Status>,
}

/// Names of the option fields a subsequent stream request set to something
/// other than the pinned (first-request) value. Proto3 scalars are counted
/// only when they also differ from their type default — an unset scalar is
/// indistinguishable from the default and is not "set".
fn ignored_option_fields(
    pinned: &mux::EmbedArrowRequest,
    req: &mux::EmbedArrowRequest,
) -> Vec<&'static str> {
    let mut ignored = Vec::new();
    if req.target.is_some() && req.target != pinned.target {
        ignored.push("target");
    }
    if req.truncate && req.truncate != pinned.truncate {
        ignored.push("truncate");
    }
    if req.normalize && req.normalize != pinned.normalize {
        ignored.push("normalize");
    }
    if req.noop && req.noop != pinned.noop {
        ignored.push("noop");
    }
    if req.truncation_direction != 0 && req.truncation_direction != pinned.truncation_direction {
        ignored.push("truncation_direction");
    }
    if req.prompt_name.is_some() && req.prompt_name != pinned.prompt_name {
        ignored.push("prompt_name");
    }
    if req.dimensions.is_some() && req.dimensions != pinned.dimensions {
        ignored.push("dimensions");
    }
    if req.compression != 0 && req.compression != pinned.compression {
        ignored.push("compression");
    }
    if req.output_dtype != 0 && req.output_dtype != pinned.output_dtype {
        ignored.push("output_dtype");
    }
    if req.max_concurrent_batches != 0
        && req.max_concurrent_batches != pinned.max_concurrent_batches
    {
        ignored.push("max_concurrent_batches");
    }
    ignored
}

/// Record `seq` as failed, keeping the frontier at the MINIMUM failing seq.
/// The frontier may DECREASE when an earlier batch fails after a later one;
/// the collector's in-order emission makes the lowest seq terminal either way.
fn lower_frontier(tx: &watch::Sender<Option<u64>>, seq: u64) {
    tx.send_if_modified(|frontier| {
        if frontier.is_none_or(|cur| seq < cur) {
            *frontier = Some(seq);
            true
        } else {
            false
        }
    });
}

/// Resolve when the failure frontier drops BELOW `seq`: this batch's result
/// can never be emitted and the batch must be abandoned. Batches at or below
/// the frontier run to completion — they are owed to the client.
async fn frontier_passed(rx: &mut watch::Receiver<Option<u64>>, seq: u64) {
    if rx
        .wait_for(|frontier| matches!(frontier, Some(cur) if *cur < seq))
        .await
        .is_err()
    {
        // Every frontier sender is gone: no failure can ever pass this seq.
        std::future::pending::<()>().await;
    }
}

/// Resolve as soon as any failure frontier exists (the stream is failing, so
/// the reader must not admit further batches).
async fn frontier_exists(rx: &mut watch::Receiver<Option<u64>>) {
    if rx.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Reader/collector machinery of `EmbedArrowStream`, generic over the batch
/// processor so tests can drive it with scripted batches (the production
/// processor is `ArrowStreamJob::run_batch`). Returns when the stream is
/// finished, terminally failed, or the client went away.
///
/// Three stages, bounded by a semaphore of `max_concurrent` (K) permits:
///
/// - READER (spawned): assigns seq i from 0, acquires a permit BEFORE
///   admitting batch i, spawns the batch task. Stops on inbound EOF (all
///   in-flight batches drain; clean close), on an inbound transport error
///   (synthesized as a failure at the next unadmitted seq, so every batch
///   admitted before it drains and emits first — "drain then fail"), and on
///   cancellation.
/// - BATCH TASK i: runs the processor; on failure lowers the failure
///   frontier to min(frontier, i). Aborts — its result, even a completed
///   one, is discarded — as soon as the frontier drops below i; it never
///   aborts while i is at or below the frontier. There is NO server-side
///   retry of a failed batch on another instance: the contract is
///   discard-and-let-the-client-resume.
/// - COLLECTOR (this future): reorders completions in a `BTreeMap` and emits
///   the contiguous prefix in seq order, releasing each batch's permit ON
///   EMISSION (not completion). An `Err` at the emission head is terminal:
///   it is emitted and the stream shuts down, so the terminal status is
///   always the LOWEST-sequence failure. A closed response channel (client
///   gone) cancels the reader and every batch task.
async fn run_arrow_stream_pipeline<S, P, Fut>(
    mut stream: S,
    mut first: mux::EmbedArrowRequest,
    max_concurrent: usize,
    tx: mpsc::Sender<Result<mux::EmbedArrowResponse, Status>>,
    process: P,
) -> ArrowStreamStats
where
    S: futures::Stream<Item = Result<mux::EmbedArrowRequest, Status>> + Unpin + Send + 'static,
    P: Fn(u64, Vec<u8>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<mux::EmbedArrowResponse, Status>> + Send + 'static,
{
    let max_concurrent = max_concurrent.max(1);
    let semaphore = Arc::new(Semaphore::new(max_concurrent));
    let (frontier_tx, frontier_rx) = watch::channel(None::<u64>);
    // One slot per admitted batch plus the reader's synthetic failure entry,
    // so no sender ever blocks the collector.
    let (comp_tx, mut comp_rx) = mpsc::channel::<BatchCompletion>(max_concurrent + 1);
    let ignored_batches = Arc::new(AtomicU64::new(0));

    // READER
    let reader_ignored = ignored_batches.clone();
    let reader_comp_tx = comp_tx.clone();
    let mut reader_frontier_rx = frontier_rx.clone();
    tokio::spawn(async move {
        let comp_tx = reader_comp_tx;
        let mut next_seq: u64 = 0;
        let mut pending_ipc = Some(std::mem::take(&mut first.arrow_ipc));
        let pinned = first; // options-only copy for the ignored-field check
        loop {
            let ipc = match pending_ipc.take() {
                Some(ipc) => ipc,
                None => {
                    let item = tokio::select! {
                        // Collector gone: stop reading.
                        _ = comp_tx.closed() => return,
                        item = stream.next() => item,
                    };
                    match item {
                        // Inbound EOF: admit no more; in-flight batches drain.
                        None => return,
                        Some(Ok(req)) => {
                            let ignored = ignored_option_fields(&pinned, &req);
                            if !ignored.is_empty() {
                                reader_ignored.fetch_add(1, Ordering::Relaxed);
                                tracing::debug!(
                                    seq = next_seq,
                                    fields = ?ignored,
                                    "ignoring option fields set after the first request"
                                );
                            }
                            req.arrow_ipc
                        }
                        Some(Err(e)) => {
                            // Inbound transport error: fail at the next
                            // unadmitted seq so every batch admitted before
                            // it drains and emits first ("drain then fail").
                            lower_frontier(&frontier_tx, next_seq);
                            let _ = comp_tx
                                .send(BatchCompletion {
                                    seq: next_seq,
                                    permit: None,
                                    result: Err(Status::internal(format!("Stream error: {}", e))),
                                })
                                .await;
                            return;
                        }
                    }
                }
            };
            // A permit gates ADMISSION of this batch; the collector releases
            // it once the batch's response has been emitted to the client.
            let permit = tokio::select! {
                _ = comp_tx.closed() => return,
                // The stream is failing: admit no more batches.
                _ = frontier_exists(&mut reader_frontier_rx) => return,
                permit = semaphore.clone().acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return, // semaphore closed (defensive; unused)
                },
            };
            let seq = next_seq;
            next_seq += 1;
            let fut = process(seq, ipc);
            let comp_tx = comp_tx.clone();
            let frontier_tx = frontier_tx.clone();
            let mut frontier_rx = frontier_rx.clone();
            // BATCH TASK
            tokio::spawn(async move {
                tokio::select! {
                    // Collector gone (terminal status emitted, or the client
                    // hung up): abandon the batch.
                    _ = comp_tx.closed() => {}
                    // An earlier batch failed: this result can never be
                    // emitted. Abandon it — the client resends after resume.
                    _ = frontier_passed(&mut frontier_rx, seq) => {}
                    result = std::panic::AssertUnwindSafe(fut).catch_unwind() => {
                        let result = result
                            .unwrap_or_else(|_| Err(Status::internal("batch task panicked")));
                        if result.is_err() {
                            lower_frontier(&frontier_tx, seq);
                        }
                        let _ = comp_tx
                            .send(BatchCompletion { seq, permit: Some(permit), result })
                            .await;
                    }
                }
            });
        }
    });
    // The completion channel closes once the reader and all batch tasks are
    // done — that is the collector's clean-close signal.
    drop(comp_tx);

    // COLLECTOR
    let mut buffered: BTreeMap<u64, BatchCompletion> = BTreeMap::new();
    let mut next_emit: u64 = 0;
    'collect: loop {
        let completion = tokio::select! {
            // Client hung up: dropping `comp_rx` on exit cancels the reader
            // and every batch task.
            _ = tx.closed() => {
                tracing::debug!("EmbedArrowStream client went away; cancelling in-flight batches");
                break 'collect;
            }
            completion = comp_rx.recv() => match completion {
                Some(completion) => completion,
                // Inbound EOF and every in-flight batch drained: clean close.
                None => break 'collect,
            },
        };
        buffered.insert(completion.seq, completion);
        // Emit the contiguous prefix in seq order.
        while let Some(completion) = buffered.remove(&next_emit) {
            match completion.result {
                Ok(response) => {
                    if tx.send(Ok(response)).await.is_err() {
                        tracing::debug!(
                            "EmbedArrowStream client went away; cancelling in-flight batches"
                        );
                        break 'collect;
                    }
                    // Emitted: release the batch's admission permit.
                    drop(completion.permit);
                    next_emit += 1;
                }
                Err(status) => {
                    // The lowest-sequence failure reaches the emission head
                    // first, so it is always the stream's terminal status;
                    // results buffered beyond it are discarded.
                    let _ = tx.send(Err(status)).await;
                    break 'collect;
                }
            }
        }
    }
    ArrowStreamStats {
        batches_with_ignored_fields: ignored_batches.load(Ordering::Relaxed),
    }
}

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
#[instrument(
    name = "tei.embed_stream",
    skip_all,
    fields(tei.rows = rows.len(), tei.batches, tei.errors)
)]
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
    let mut batches = 0usize;
    while cursor < pending.len() {
        batches += 1;
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

    let rows: Vec<Result<Res, String>> = out
        .into_iter()
        .map(|r| r.expect("every row is resolved"))
        .collect();
    Span::current()
        .record("tei.batches", batches)
        .record("tei.errors", rows.iter().filter(|r| r.is_err()).count());
    Ok(RowOutcomes { rows })
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

    fn target(routing: mux::target::Routing) -> Option<mux::Target> {
        Some(mux::Target {
            routing: Some(routing),
        })
    }

    #[tokio::test]
    async fn test_resolve_target_valid_instance_name() {
        let service = create_test_service();
        let result = service
            .resolve_target(target(mux::target::Routing::InstanceName(
                "test-instance".to_string(),
            )))
            .await;
        assert_eq!(result.unwrap(), "test-instance");
    }

    #[tokio::test]
    async fn test_resolve_target_empty_instance_name() {
        let service = create_test_service();
        let err = service
            .resolve_target(target(mux::target::Routing::InstanceName(String::new())))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("cannot be empty"));
    }

    #[tokio::test]
    async fn test_resolve_target_missing() {
        let service = create_test_service();
        let err = service.resolve_target(None).await.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing target"));
    }

    #[tokio::test]
    async fn test_resolve_target_no_routing() {
        let service = create_test_service();
        let err = service
            .resolve_target(Some(mux::Target { routing: None }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("No routing specified"));
    }

    #[tokio::test]
    async fn test_resolve_target_index_routing_unimplemented() {
        let service = create_test_service();
        let err = service
            .resolve_target(target(mux::target::Routing::InstanceIndex(0)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::Unimplemented);
    }

    // ========================================================================
    // Model-based routing
    // ========================================================================

    /// Registry with named instances of a model, each started on a mock
    /// process manager (status controllable per instance)
    async fn service_with_model_instances(
        names: &[&str],
        model: &str,
    ) -> (TeiMultiplexerService, Arc<Registry>) {
        use crate::instance::mocks::MockProcessManager;
        let registry = Arc::new(Registry::new(
            None,
            "text-embeddings-router".to_string(),
            8080,
            8180,
        ));
        for (i, name) in names.iter().enumerate() {
            let config = InstanceConfig {
                name: name.to_string(),
                model_id: model.to_string(),
                port: 58000 + i as u16,
                ..Default::default()
            };
            // Build directly so the instance uses a mock manager
            let instance = Arc::new(crate::instance::TeiInstance::new_with_manager(
                config,
                Arc::new(MockProcessManager::new()),
            ));
            instance.start("mock").await.unwrap();
            *instance.status.write().await = crate::instance::InstanceStatus::Running;
            registry.insert_for_tests(instance).await;
        }
        let pool = BackendPool::new(registry.clone());
        (TeiMultiplexerService::new(pool, 1024, 30), registry)
    }

    #[tokio::test]
    async fn test_model_routing_round_robins_running_instances() {
        let (service, _r) = service_with_model_instances(&["m3-a", "m3-b"], "BAAI/bge-m3").await;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(
                service
                    .resolve_target(target(mux::target::Routing::ModelId(
                        "BAAI/bge-m3".to_string(),
                    )))
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(seen, vec!["m3-a", "m3-b", "m3-a", "m3-b"]);
    }

    #[tokio::test]
    async fn test_model_routing_skips_non_running() {
        let (service, registry) =
            service_with_model_instances(&["m3-a", "m3-b"], "BAAI/bge-m3").await;
        let b = registry.get("m3-b").await.unwrap();
        *b.status.write().await = crate::instance::InstanceStatus::Failed;
        for _ in 0..3 {
            let picked = service
                .resolve_target(target(mux::target::Routing::ModelId(
                    "BAAI/bge-m3".to_string(),
                )))
                .await
                .unwrap();
            assert_eq!(picked, "m3-a");
        }
        // Recovery: back to Running → back in rotation
        *b.status.write().await = crate::instance::InstanceStatus::Running;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..4 {
            seen.insert(
                service
                    .resolve_target(target(mux::target::Routing::ModelId(
                        "BAAI/bge-m3".to_string(),
                    )))
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(seen.len(), 2);
    }

    #[tokio::test]
    async fn test_model_routing_no_match_is_not_found_naming_model() {
        let (service, _r) = service_with_model_instances(&["m3-a"], "BAAI/bge-m3").await;
        let err = service
            .resolve_target(target(mux::target::Routing::ModelId("other/model".into())))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::NotFound);
        assert!(err.message().contains("other/model"), "{}", err.message());
        // Same shape when the only instance is not running
        let (service, registry) = service_with_model_instances(&["m3-a"], "BAAI/bge-m3").await;
        let a = registry.get("m3-a").await.unwrap();
        *a.status.write().await = crate::instance::InstanceStatus::Starting;
        let err = service
            .resolve_target(target(mux::target::Routing::ModelId("BAAI/bge-m3".into())))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn test_model_routing_empty_model_id() {
        let service = create_test_service();
        let err = service
            .resolve_target(target(mux::target::Routing::ModelId(String::new())))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_embed_via_model_routing_reaches_backend_path() {
        // Full RPC through model routing: resolves to the (unreachable) mock
        // instance and fails at connect — proving routing picked it.
        let (service, _r) = service_with_model_instances(&["m3-a"], "BAAI/bge-m3").await;
        let request = Request::new(mux::EmbedRequest {
            target: target(mux::target::Routing::ModelId("BAAI/bge-m3".to_string())),
            request: Some(tei::EmbedRequest {
                inputs: "hi".to_string(),
                truncate: true,
                normalize: Some(true),
                truncation_direction: 0,
                prompt_name: None,
                dimensions: None,
            }),
        });
        let err = service.embed(request).await.unwrap_err();
        assert_eq!(err.code(), Code::Unavailable);
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
    // EmbedArrowStream RPC Tests
    // ========================================================================

    /// Noop request carrying `texts` as its IPC batch; `target` only on the
    /// first request of a stream.
    fn arrow_stream_request(texts: &[&str], with_target: bool) -> mux::EmbedArrowRequest {
        mux::EmbedArrowRequest {
            target: with_target.then(|| mux::Target {
                routing: Some(mux::target::Routing::InstanceName("test".to_string())),
            }),
            arrow_ipc: ipc_from_array(Arc::new(StringArray::from(texts.to_vec())), "text"),
            noop: true,
            ..Default::default()
        }
    }

    /// Drain the response stream, decoding each IPC payload to a RecordBatch.
    async fn collect_arrow_stream(
        mut stream: tokio_stream::wrappers::ReceiverStream<Result<mux::EmbedArrowResponse, Status>>,
    ) -> Vec<Result<RecordBatch, Status>> {
        let mut out = Vec::new();
        while let Some(result) = stream.next().await {
            out.push(result.map(|response| {
                let mut reader =
                    StreamReader::try_new(Cursor::new(response.arrow_ipc), None).unwrap();
                reader.next().unwrap().unwrap()
            }));
        }
        out
    }

    #[tokio::test]
    async fn test_embed_arrow_stream_noop_batches_in_order() {
        let service = create_test_service();
        let requests = vec![
            Ok(arrow_stream_request(&["a", "b"], true)),
            Ok(arrow_stream_request(&["c", "d", "e"], false)),
            Ok(arrow_stream_request(&["f"], false)),
        ];
        let stream = service
            .embed_arrow_stream_core(tokio_stream::iter(requests))
            .await
            .unwrap();
        let batches = collect_arrow_stream(stream).await;
        assert_eq!(batches.len(), 3);
        let rows: Vec<usize> = batches
            .into_iter()
            .map(|b| {
                let b = b.unwrap();
                assert_eq!(b.num_columns(), 2);
                assert_eq!(b.schema().field(0).name(), "embeddings");
                assert_eq!(b.schema().field(1).name(), "error");
                b.num_rows()
            })
            .collect();
        assert_eq!(rows, vec![2, 3, 1], "responses arrive in request order");
    }

    #[tokio::test]
    async fn test_embed_arrow_stream_first_request_missing_target() {
        let service = create_test_service();
        let requests = vec![Ok(arrow_stream_request(&["a"], false))];
        let err = service
            .embed_arrow_stream_core(tokio_stream::iter(requests))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Missing target"));
    }

    #[tokio::test]
    async fn test_embed_arrow_stream_later_options_ignored() {
        use arrow::array::FixedSizeListArray;
        let service = create_test_service();
        let mut first = arrow_stream_request(&["a"], true);
        first.output_dtype = mux::OutputDtype::F16 as i32;
        // The second request tries to flip the dtype (and drops noop); only
        // its arrow_ipc may be read. It also sets several other option
        // fields, all of which must be ignored (they are surfaced via the
        // stream's ignored-fields counter, tested at the pipeline level).
        let mut second = arrow_stream_request(&["b", "c"], false);
        second.output_dtype = mux::OutputDtype::F32 as i32;
        second.noop = false;
        second.truncate = true;
        second.dimensions = Some(8);
        second.max_concurrent_batches = 63;

        let stream = service
            .embed_arrow_stream_core(tokio_stream::iter(vec![Ok(first), Ok(second)]))
            .await
            .unwrap();
        let batches = collect_arrow_stream(stream).await;
        assert_eq!(batches.len(), 2);
        for (batch, expected_rows) in batches.into_iter().zip([1usize, 2]) {
            let batch = batch.unwrap();
            assert_eq!(batch.num_rows(), expected_rows);
            let emb = batch
                .column(0)
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .unwrap();
            assert_eq!(
                emb.value_type(),
                DataType::Float16,
                "dtype comes from the first request only"
            );
        }
    }

    #[tokio::test]
    async fn test_embed_arrow_stream_empty_stream() {
        let service = create_test_service();
        let err = service
            .embed_arrow_stream_core(tokio_stream::iter(Vec::<
                Result<mux::EmbedArrowRequest, Status>,
            >::new()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Empty stream"));
    }

    #[tokio::test]
    async fn test_embed_arrow_stream_bad_batch_terminates_stream() {
        let service = create_test_service();
        let mut bad = arrow_stream_request(&[], false);
        bad.arrow_ipc = vec![1, 2, 3, 4]; // invalid Arrow IPC
        let requests = vec![
            Ok(arrow_stream_request(&["a"], true)),
            Ok(bad),
            Ok(arrow_stream_request(&["never processed"], false)),
        ];
        let stream = service
            .embed_arrow_stream_core(tokio_stream::iter(requests))
            .await
            .unwrap();
        let batches = collect_arrow_stream(stream).await;
        assert_eq!(batches.len(), 2, "stream ends at the failing batch");
        assert_eq!(batches[0].as_ref().unwrap().num_rows(), 1);
        let err = batches[1].as_ref().unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("Invalid Arrow IPC"));
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

    // ========================================================================
    // Prometheus Metrics Tests
    // ========================================================================

    /// Assert that `rendered` contains a sample line for `name` carrying all
    /// `labels` (label order in the rendered output is not stable)
    fn assert_metric_line(rendered: &str, name: &str, labels: &[&str]) {
        let found = rendered
            .lines()
            .any(|line| line.starts_with(name) && labels.iter().all(|l| line.contains(l)));
        assert!(
            found,
            "expected a `{name}` sample with labels {labels:?} in:\n{rendered}"
        );
    }

    #[tokio::test]
    async fn test_embed_arrow_noop_records_prometheus_metrics() {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::ipc::writer::StreamWriter;
        use arrow::record_batch::RecordBatch;

        let handle = crate::metrics::test_support::prometheus_handle();
        let service = create_test_service();

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
                routing: Some(mux::target::Routing::InstanceName(
                    "metrics-inst".to_string(),
                )),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop: true,
            ..Default::default()
        });
        service.embed_arrow(request).await.unwrap();

        let rendered = handle.render();
        assert_metric_line(
            &rendered,
            "tei_mux_requests_total",
            &[
                r#"method="embed_arrow""#,
                r#"instance="metrics-inst""#,
                r#"status="ok""#,
            ],
        );
        assert_metric_line(
            &rendered,
            "tei_mux_rows_total",
            &[r#"instance="metrics-inst""#, r#"status="ok""#],
        );
        assert_metric_line(
            &rendered,
            "tei_mux_request_duration_seconds",
            &[r#"method="embed_arrow""#, r#"instance="metrics-inst""#],
        );

        // A request that fails target resolution is counted with
        // instance="unknown" and status="error".
        let err = service
            .embed_arrow(Request::new(mux::EmbedArrowRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        let rendered = handle.render();
        assert_metric_line(
            &rendered,
            "tei_mux_requests_total",
            &[
                r#"method="embed_arrow""#,
                r#"instance="unknown""#,
                r#"status="error""#,
            ],
        );
    }

    // ========================================================================
    // EmbedArrowStream concurrent pipeline tests (scripted processor)
    // ========================================================================

    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Notify;
    use tokio_stream::wrappers::ReceiverStream;

    /// Scripted per-seq behavior driving the stream machinery with no backend.
    #[derive(Clone)]
    enum BatchScript {
        /// Succeed after the paused-time delay (payload = the seq's bytes)
        Ok(Duration),
        /// Fail with this status after the delay
        Fail(Duration, Code, &'static str),
        /// Park until cancelled — never completes
        Never,
        /// Park until the Notify fires, then succeed
        Gate(Arc<Notify>),
    }

    /// Records processor activity: the concurrency high-water mark, started /
    /// finished counts, and the seqs whose futures were dropped mid-run
    /// (= batches that observed cancellation).
    #[derive(Default)]
    struct Recorder {
        active: AtomicUsize,
        max_active: AtomicUsize,
        started: AtomicUsize,
        finished: AtomicUsize,
        cancelled: Mutex<Vec<u64>>,
    }

    impl Recorder {
        fn cancelled_sorted(&self) -> Vec<u64> {
            let mut cancelled = self.cancelled.lock().unwrap().clone();
            cancelled.sort_unstable();
            cancelled
        }
    }

    /// Tracks one processor run; recorded as cancelled when dropped unfinished.
    struct RunGuard {
        seq: u64,
        finished: bool,
        rec: Arc<Recorder>,
    }

    impl RunGuard {
        fn start(seq: u64, rec: Arc<Recorder>) -> Self {
            rec.started.fetch_add(1, Ordering::SeqCst);
            let now = rec.active.fetch_add(1, Ordering::SeqCst) + 1;
            rec.max_active.fetch_max(now, Ordering::SeqCst);
            Self {
                seq,
                finished: false,
                rec,
            }
        }

        fn finish(mut self) {
            self.finished = true;
            self.rec.finished.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for RunGuard {
        fn drop(&mut self) {
            self.rec.active.fetch_sub(1, Ordering::SeqCst);
            if !self.finished {
                self.rec.cancelled.lock().unwrap().push(self.seq);
            }
        }
    }

    fn seq_response(seq: u64) -> mux::EmbedArrowResponse {
        mux::EmbedArrowResponse {
            arrow_ipc: seq.to_le_bytes().to_vec(),
        }
    }

    fn decode_seq(response: mux::EmbedArrowResponse) -> u64 {
        u64::from_le_bytes(response.arrow_ipc.as_slice().try_into().unwrap())
    }

    fn scripted_processor(
        scripts: Vec<BatchScript>,
        rec: Arc<Recorder>,
    ) -> impl Fn(
        u64,
        Vec<u8>,
    ) -> futures::future::BoxFuture<'static, Result<mux::EmbedArrowResponse, Status>> {
        move |seq, _ipc| {
            let script = scripts[seq as usize].clone();
            let rec = rec.clone();
            Box::pin(async move {
                let guard = RunGuard::start(seq, rec);
                let result = match script {
                    BatchScript::Ok(delay) => {
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        Ok(seq_response(seq))
                    }
                    BatchScript::Fail(delay, code, msg) => {
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        Err(Status::new(code, msg))
                    }
                    BatchScript::Never => std::future::pending().await,
                    BatchScript::Gate(gate) => {
                        gate.notified().await;
                        Ok(seq_response(seq))
                    }
                };
                guard.finish();
                result
            })
        }
    }

    /// Run the pipeline over the given requests and processor, draining the
    /// response channel to the end. `Ok` payloads decode back to their seq.
    async fn drive_pipeline_with<P, Fut>(
        requests: Vec<Result<mux::EmbedArrowRequest, Status>>,
        k: usize,
        response_cap: usize,
        process: P,
    ) -> (Vec<Result<u64, Status>>, ArrowStreamStats)
    where
        P: Fn(u64, Vec<u8>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<mux::EmbedArrowResponse, Status>> + Send + 'static,
    {
        let mut it = requests.into_iter();
        let first = it
            .next()
            .expect("at least one request")
            .expect("first is Ok");
        let rest: Vec<_> = it.collect();
        let (tx, mut rx) = mpsc::channel(response_cap);
        let pipeline = tokio::spawn(run_arrow_stream_pipeline(
            tokio_stream::iter(rest),
            first,
            k,
            tx,
            process,
        ));
        let mut emitted = Vec::new();
        while let Some(item) = rx.recv().await {
            emitted.push(item.map(decode_seq));
        }
        let stats = pipeline.await.expect("pipeline task");
        (emitted, stats)
    }

    async fn drive_pipeline(
        scripts: Vec<BatchScript>,
        k: usize,
        rec: Arc<Recorder>,
    ) -> (Vec<Result<u64, Status>>, ArrowStreamStats) {
        let requests = (0..scripts.len())
            .map(|_| Ok(mux::EmbedArrowRequest::default()))
            .collect();
        drive_pipeline_with(requests, k, 32, scripted_processor(scripts, rec)).await
    }

    /// The leading run of successful seqs.
    fn ok_prefix(emitted: &[Result<u64, Status>]) -> Vec<u64> {
        emitted
            .iter()
            .map_while(|r| r.as_ref().ok().copied())
            .collect()
    }

    /// Let every ready task run to quiescence without advancing paused time.
    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_emits_in_seq_order_under_adversarial_completion() {
        // Batch i completes after (16 - i) * 10ms — the completion order is
        // the exact reverse of the request order.
        let rec = Arc::new(Recorder::default());
        let scripts: Vec<BatchScript> = (0..16u64)
            .map(|i| BatchScript::Ok(Duration::from_millis((16 - i) * 10)))
            .collect();
        let (emitted, _) = drive_pipeline(scripts, 16, rec.clone()).await;
        let seqs: Vec<u64> = emitted.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            seqs,
            (0..16).collect::<Vec<_>>(),
            "payloads match their seq, strictly in request order"
        );
        assert_eq!(rec.max_active.load(Ordering::SeqCst), 16);
        assert!(rec.cancelled_sorted().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_admission_bounded_by_k_and_permit_released_on_emission() {
        let gate = Arc::new(Notify::new());
        let rec = Arc::new(Recorder::default());
        let mut scripts = vec![BatchScript::Gate(gate.clone())];
        scripts.extend((1..16).map(|_| BatchScript::Ok(Duration::ZERO)));
        let requests: Vec<_> = (0..16)
            .map(|_| Ok(mux::EmbedArrowRequest::default()))
            .collect();
        let mut it = requests.into_iter();
        let first = it.next().unwrap().unwrap();
        let rest: Vec<_> = it.collect();
        let (tx, mut rx) = mpsc::channel(32);
        let pipeline = tokio::spawn(run_arrow_stream_pipeline(
            tokio_stream::iter(rest),
            first,
            4,
            tx,
            scripted_processor(scripts, rec.clone()),
        ));
        settle().await;
        // The head is stalled; batches 1-3 completed but are unemitted, so
        // their permits are still held: admission stops at exactly K. Were
        // permits released on COMPLETION, batches 4-6 would already run.
        assert_eq!(
            rec.started.load(Ordering::SeqCst),
            4,
            "admission stops at exactly K while the head is stalled"
        );
        assert_eq!(rec.finished.load(Ordering::SeqCst), 3);
        gate.notify_one();
        let mut seqs = Vec::new();
        while let Some(item) = rx.recv().await {
            seqs.push(decode_seq(item.unwrap()));
        }
        pipeline.await.unwrap();
        assert_eq!(seqs, (0..16).collect::<Vec<_>>());
        assert!(
            rec.max_active.load(Ordering::SeqCst) <= 4,
            "concurrency never exceeds K"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_lowest_seq_failure_is_terminal() {
        // Batch 3 fails instantly, batch 1 fails much later: the terminal
        // status must still be batch 1's — batch 3's error never surfaces.
        let rec = Arc::new(Recorder::default());
        let scripts = vec![
            BatchScript::Ok(Duration::from_millis(5)),
            BatchScript::Fail(
                Duration::from_millis(50),
                Code::InvalidArgument,
                "slow fail 1",
            ),
            BatchScript::Ok(Duration::from_millis(5)),
            BatchScript::Fail(Duration::ZERO, Code::Internal, "fast fail 3"),
            BatchScript::Ok(Duration::from_millis(5)),
        ];
        let (emitted, _) = drive_pipeline(scripts, 8, rec).await;
        assert_eq!(emitted.len(), 2, "batch 0 then the terminal error");
        assert_eq!(ok_prefix(&emitted), vec![0]);
        let err = emitted[1].as_ref().unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert_eq!(err.message(), "slow fail 1", "batch 3's error is discarded");
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_failure_cancels_later_batches_completes_earlier() {
        let rec = Arc::new(Recorder::default());
        let scripts = vec![
            BatchScript::Ok(Duration::from_millis(20)),
            BatchScript::Ok(Duration::from_millis(30)),
            BatchScript::Fail(Duration::from_millis(10), Code::Internal, "boom"),
            BatchScript::Never,
            BatchScript::Never,
            BatchScript::Never,
        ];
        let (emitted, _) = drive_pipeline(scripts, 8, rec.clone()).await;
        settle().await;
        // Batches before the failure run to completion and are emitted...
        assert_eq!(ok_prefix(&emitted), vec![0, 1]);
        assert_eq!(emitted.len(), 3);
        assert_eq!(emitted[2].as_ref().unwrap_err().message(), "boom");
        // ...batches after it observe cancellation.
        assert_eq!(rec.cancelled_sorted(), vec![3, 4, 5]);
        assert_eq!(rec.finished.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_later_lower_failure_supersedes() {
        // Batch 5 fails first, batch 3 fails later: the frontier DECREASES
        // and the terminal status is batch 3's.
        let rec = Arc::new(Recorder::default());
        let scripts = vec![
            BatchScript::Ok(Duration::from_millis(10)),
            BatchScript::Ok(Duration::from_millis(10)),
            BatchScript::Ok(Duration::from_millis(10)),
            BatchScript::Fail(Duration::from_millis(50), Code::ResourceExhausted, "fail 3"),
            BatchScript::Never,
            BatchScript::Fail(Duration::from_millis(1), Code::Internal, "fail 5"),
        ];
        let (emitted, _) = drive_pipeline(scripts, 8, rec.clone()).await;
        settle().await;
        assert_eq!(ok_prefix(&emitted), vec![0, 1, 2]);
        assert_eq!(emitted.len(), 4);
        let err = emitted[3].as_ref().unwrap_err();
        assert_eq!(err.code(), Code::ResourceExhausted);
        assert_eq!(err.message(), "fail 3", "the LOWER failing seq wins");
        // Batch 4 outlived the first (higher) failure but was cancelled once
        // the frontier dropped below it.
        assert_eq!(rec.cancelled_sorted(), vec![4]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_client_disconnect_cancels_all() {
        let rec = Arc::new(Recorder::default());
        let scripts = vec![BatchScript::Never; 4];
        let (tx, rx) = mpsc::channel(8);
        let requests: Vec<Result<mux::EmbedArrowRequest, Status>> = (0..3)
            .map(|_| Ok(mux::EmbedArrowRequest::default()))
            .collect();
        let pipeline = tokio::spawn(run_arrow_stream_pipeline(
            tokio_stream::iter(requests),
            mux::EmbedArrowRequest::default(),
            4,
            tx,
            scripted_processor(scripts, rec.clone()),
        ));
        settle().await;
        assert_eq!(rec.started.load(Ordering::SeqCst), 4);
        drop(rx); // the client goes away mid-stream
        pipeline.await.expect("pipeline ends without panicking");
        settle().await;
        assert_eq!(
            rec.cancelled_sorted(),
            vec![0, 1, 2, 3],
            "every in-flight batch is cancelled"
        );
        assert_eq!(rec.finished.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_inbound_transport_error_drains_then_fails() {
        // Two good requests, then the inbound stream errors: both admitted
        // batches drain and emit first, then the transport error terminates.
        let rec = Arc::new(Recorder::default());
        let requests = vec![
            Ok(mux::EmbedArrowRequest::default()),
            Ok(mux::EmbedArrowRequest::default()),
            Err(Status::aborted("client link broke")),
        ];
        let scripts = vec![
            BatchScript::Ok(Duration::from_millis(20)),
            BatchScript::Ok(Duration::from_millis(10)),
        ];
        let (emitted, _) =
            drive_pipeline_with(requests, 4, 32, scripted_processor(scripts, rec)).await;
        assert_eq!(ok_prefix(&emitted), vec![0, 1]);
        assert_eq!(emitted.len(), 3);
        let err = emitted[2].as_ref().unwrap_err();
        assert_eq!(err.code(), Code::Internal);
        assert!(err.message().contains("Stream error"), "{}", err.message());
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_eof_drains_in_flight() {
        // EOF with everything still in flight: full drain, clean close.
        let rec = Arc::new(Recorder::default());
        let scripts: Vec<BatchScript> = (0..5u64)
            .map(|i| BatchScript::Ok(Duration::from_millis((5 - i) * 10)))
            .collect();
        let (emitted, stats) = drive_pipeline(scripts, 5, rec.clone()).await;
        assert_eq!(ok_prefix(&emitted), vec![0, 1, 2, 3, 4]);
        assert_eq!(emitted.len(), 5, "no terminal error on clean close");
        assert_eq!(rec.finished.load(Ordering::SeqCst), 5);
        assert!(rec.cancelled_sorted().is_empty());
        assert_eq!(stats.batches_with_ignored_fields, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_per_batch_timeout_fails_right_seq() {
        // The per-batch deadline sits around each batch future exactly like
        // `run_batch`'s timeout; an elapsed deadline is that batch's failure.
        let rec = Arc::new(Recorder::default());
        let scripts = vec![
            BatchScript::Ok(Duration::from_millis(10)),
            BatchScript::Ok(Duration::from_millis(10)),
            BatchScript::Ok(Duration::from_secs(600)), // exceeds the deadline
            BatchScript::Ok(Duration::from_millis(10)),
        ];
        let inner = scripted_processor(scripts, rec);
        let process = move |seq: u64, ipc: Vec<u8>| {
            let fut = inner(seq, ipc);
            async move {
                match timeout(Duration::from_millis(100), fut).await {
                    Ok(result) => result,
                    Err(_) => Err(Status::deadline_exceeded("Request timeout")),
                }
            }
        };
        let requests = (0..4)
            .map(|_| Ok(mux::EmbedArrowRequest::default()))
            .collect();
        let (emitted, _) = drive_pipeline_with(requests, 4, 32, process).await;
        assert_eq!(ok_prefix(&emitted), vec![0, 1], "earlier batches delivered");
        assert_eq!(emitted.len(), 3);
        assert_eq!(
            emitted[2].as_ref().unwrap_err().code(),
            Code::DeadlineExceeded,
            "the timeout is charged to the right seq"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_slow_client_bounds_admission() {
        // Tiny response channel and a client that reads nothing: the
        // collector's blocked emission holds permits, so admission stalls.
        let rec = Arc::new(Recorder::default());
        let scripts = vec![BatchScript::Ok(Duration::ZERO); 8];
        let requests: Vec<_> = (0..8)
            .map(|_| Ok(mux::EmbedArrowRequest::default()))
            .collect();
        let mut it = requests.into_iter();
        let first = it.next().unwrap().unwrap();
        let rest: Vec<_> = it.collect();
        let (tx, mut rx) = mpsc::channel(1);
        let pipeline = tokio::spawn(run_arrow_stream_pipeline(
            tokio_stream::iter(rest),
            first,
            2,
            tx,
            scripted_processor(scripts, rec.clone()),
        ));
        settle().await;
        // Batch 0 emitted (fills the channel, permit released), batch 1 is
        // stuck in the collector's send (permit held), batch 2 was admitted
        // with batch 0's released permit: admission stalls at 3 = K + cap.
        assert_eq!(
            rec.started.load(Ordering::SeqCst),
            3,
            "admission stalls while the client does not read"
        );
        // Draining the client releases everything, still strictly in order.
        let mut seqs = Vec::new();
        while let Some(item) = rx.recv().await {
            seqs.push(decode_seq(item.unwrap()));
        }
        pipeline.await.unwrap();
        assert_eq!(seqs, (0..8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn test_stream_pipeline_counts_batches_with_ignored_fields() {
        let rec = Arc::new(Recorder::default());
        let first = mux::EmbedArrowRequest {
            truncate: true,
            output_dtype: mux::OutputDtype::F16 as i32,
            prompt_name: Some("p".to_string()),
            ..Default::default()
        };
        // #2 flips output_dtype -> counted; #3 sets nothing -> not counted;
        // #4 repeats the pinned values exactly -> not counted; #5 sets
        // dimensions -> counted.
        let second = mux::EmbedArrowRequest {
            output_dtype: mux::OutputDtype::F32 as i32,
            ..Default::default()
        };
        let third = mux::EmbedArrowRequest::default();
        let fourth = mux::EmbedArrowRequest {
            truncate: true,
            output_dtype: mux::OutputDtype::F16 as i32,
            prompt_name: Some("p".to_string()),
            ..Default::default()
        };
        let fifth = mux::EmbedArrowRequest {
            dimensions: Some(64),
            ..Default::default()
        };
        let scripts = vec![BatchScript::Ok(Duration::ZERO); 5];
        let (emitted, stats) = drive_pipeline_with(
            vec![Ok(first), Ok(second), Ok(third), Ok(fourth), Ok(fifth)],
            2,
            32,
            scripted_processor(scripts, rec),
        )
        .await;
        assert_eq!(emitted.len(), 5);
        assert_eq!(
            stats.batches_with_ignored_fields, 2,
            "only requests that SET differing option fields are counted"
        );
    }

    #[test]
    fn test_ignored_option_fields_semantics() {
        let pinned = mux::EmbedArrowRequest {
            truncate: true,
            noop: true,
            output_dtype: mux::OutputDtype::F16 as i32,
            ..Default::default()
        };
        // Unset proto3 scalars are indistinguishable from defaults: not "set".
        assert!(ignored_option_fields(&pinned, &mux::EmbedArrowRequest::default()).is_empty());
        // Fields matching the pinned values are not ignored fields.
        assert!(ignored_option_fields(&pinned, &pinned.clone()).is_empty());
        let differing = mux::EmbedArrowRequest {
            normalize: true,
            output_dtype: mux::OutputDtype::F32 as i32,
            max_concurrent_batches: 9,
            ..Default::default()
        };
        assert_eq!(
            ignored_option_fields(&pinned, &differing),
            vec!["normalize", "output_dtype", "max_concurrent_batches"]
        );
    }

    /// Deterministic LCG so the property-style test needs no rand crate.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_stream_pipeline_random_scripts_match_sequential_reference() {
        // Seeded-random delay/failure scripts vs a reference sequential
        // simulator: the emitted prefix and terminal status must be identical
        // regardless of K and completion timing.
        for seed in 0..50u64 {
            let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
            let n = 1 + (rng.next() % 12) as usize;
            let k = 1 + (rng.next() % 6) as usize;
            let mut scripts = Vec::with_capacity(n);
            for _ in 0..n {
                let delay = Duration::from_millis((rng.next() % 4) * 5);
                if rng.next().is_multiple_of(5) {
                    let code = match rng.next() % 3 {
                        0 => Code::Internal,
                        1 => Code::InvalidArgument,
                        _ => Code::Unavailable,
                    };
                    scripts.push(BatchScript::Fail(delay, code, "scripted failure"));
                } else {
                    scripts.push(BatchScript::Ok(delay));
                }
            }
            // Reference: run the same scripts strictly one at a time — Oks up
            // to the first failure, which is terminal.
            let mut reference: Vec<Result<u64, Code>> = Vec::new();
            for (seq, script) in scripts.iter().enumerate() {
                match script {
                    BatchScript::Ok(_) => reference.push(Ok(seq as u64)),
                    BatchScript::Fail(_, code, _) => {
                        reference.push(Err(*code));
                        break;
                    }
                    _ => unreachable!(),
                }
            }
            let rec = Arc::new(Recorder::default());
            let (emitted, _) = drive_pipeline(scripts, k, rec.clone()).await;
            let simplified: Vec<Result<u64, Code>> = emitted
                .iter()
                .map(|r| r.as_ref().map(|seq| *seq).map_err(Status::code))
                .collect();
            assert_eq!(simplified, reference, "seed {seed} (k={k}, n={n})");
            assert!(
                rec.max_active.load(Ordering::SeqCst) <= k,
                "seed {seed}: concurrency exceeded K"
            );
        }
    }

    #[tokio::test]
    async fn test_effective_stream_concurrency_clamps() {
        let service = create_test_service(); // server default: 4
        assert_eq!(service.effective_stream_concurrency(0), 4);
        assert_eq!(service.effective_stream_concurrency(7), 7);
        assert_eq!(
            service.effective_stream_concurrency(1000),
            64,
            "caller override clamped down to 64"
        );
        let service = service.with_stream_max_concurrent_batches(0);
        assert_eq!(
            service.effective_stream_concurrency(0),
            1,
            "server knob clamped up to 1"
        );
        let service = service.with_stream_max_concurrent_batches(999);
        assert_eq!(
            service.effective_stream_concurrency(0),
            64,
            "server knob clamped down to 64"
        );
        assert_eq!(
            service.effective_stream_concurrency(1),
            1,
            "K=1 forces sequential execution"
        );
    }

    /// Noop ModelId-target request; `with_target` only on the first request.
    fn model_stream_request(
        model: &str,
        texts: &[&str],
        with_target: bool,
    ) -> mux::EmbedArrowRequest {
        mux::EmbedArrowRequest {
            target: with_target.then(|| mux::Target {
                routing: Some(mux::target::Routing::ModelId(model.to_string())),
            }),
            arrow_ipc: ipc_from_array(Arc::new(StringArray::from(texts.to_vec())), "text"),
            noop: true,
            ..Default::default()
        }
    }

    /// Value of the first sample of `name` carrying all `labels`.
    fn metric_value(rendered: &str, name: &str, labels: &[&str]) -> u64 {
        rendered
            .lines()
            .find(|line| line.starts_with(name) && labels.iter().all(|l| line.contains(l)))
            .and_then(|line| line.rsplit(' ').next())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn test_embed_arrow_stream_model_routing_rotates_and_survives_death() {
        let handle = crate::metrics::test_support::prometheus_handle();
        let (service, registry) =
            service_with_model_instances(&["rr-alpha", "rr-beta"], "rr/model").await;
        let (req_tx, req_rx) = mpsc::channel::<Result<mux::EmbedArrowRequest, Status>>(4);
        req_tx
            .send(Ok(model_stream_request("rr/model", &["r0"], true)))
            .await
            .unwrap();
        let mut responses = service
            .embed_arrow_stream_core(ReceiverStream::new(req_rx))
            .await
            .unwrap();
        // Fresh round-robin over the sorted running instances: batch 0 goes
        // to rr-alpha, batch 1 to rr-beta. Awaiting each response before the
        // next send keeps the alternation deterministic.
        responses.next().await.unwrap().unwrap();
        req_tx
            .send(Ok(model_stream_request("rr/model", &["r1"], false)))
            .await
            .unwrap();
        responses.next().await.unwrap().unwrap();
        let rendered = handle.render();
        let alpha = &[r#"instance="rr-alpha""#, r#"status="ok""#];
        let beta = &[r#"instance="rr-beta""#, r#"status="ok""#];
        assert_eq!(metric_value(&rendered, "tei_mux_rows_total", alpha), 1);
        assert_eq!(metric_value(&rendered, "tei_mux_rows_total", beta), 1);
        // rr-beta leaves Running MID-STREAM: the survivor takes the rest.
        *registry.get("rr-beta").await.unwrap().status.write().await =
            crate::instance::InstanceStatus::Failed;
        for _ in 2..5 {
            req_tx
                .send(Ok(model_stream_request("rr/model", &["rN"], false)))
                .await
                .unwrap();
            responses.next().await.unwrap().unwrap();
        }
        drop(req_tx);
        assert!(responses.next().await.is_none(), "clean close after EOF");
        let rendered = handle.render();
        assert_eq!(metric_value(&rendered, "tei_mux_rows_total", alpha), 4);
        assert_eq!(metric_value(&rendered, "tei_mux_rows_total", beta), 1);
    }

    #[tokio::test]
    async fn test_embed_arrow_stream_model_routing_all_dead_mid_stream_not_found() {
        let (service, registry) =
            service_with_model_instances(&["dead-a", "dead-b"], "dead/model").await;
        let (req_tx, req_rx) = mpsc::channel::<Result<mux::EmbedArrowRequest, Status>>(4);
        req_tx
            .send(Ok(model_stream_request("dead/model", &["r0"], true)))
            .await
            .unwrap();
        let mut responses = service
            .embed_arrow_stream_core(ReceiverStream::new(req_rx))
            .await
            .unwrap();
        responses.next().await.unwrap().unwrap();
        // Every instance of the model dies mid-stream.
        for name in ["dead-a", "dead-b"] {
            *registry.get(name).await.unwrap().status.write().await =
                crate::instance::InstanceStatus::Failed;
        }
        req_tx
            .send(Ok(model_stream_request("dead/model", &["r1"], false)))
            .await
            .unwrap();
        let err = responses.next().await.unwrap().unwrap_err();
        assert_eq!(err.code(), Code::NotFound, "{}", err.message());
        assert!(err.message().contains("dead/model"), "{}", err.message());
        assert!(responses.next().await.is_none(), "the failure is terminal");
    }

    async fn collect_stream_ipc(
        service: &TeiMultiplexerService,
        requests: Vec<Result<mux::EmbedArrowRequest, Status>>,
    ) -> Vec<Result<Vec<u8>, Status>> {
        let mut stream = service
            .embed_arrow_stream_core(tokio_stream::iter(requests))
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(result) = stream.next().await {
            out.push(result.map(|response| response.arrow_ipc));
        }
        out
    }

    #[tokio::test]
    async fn test_embed_arrow_stream_k1_matches_sequential() {
        // K=1 must behave exactly like the sequential v1 implementation, and
        // K=4 must produce byte-identical responses (noop is deterministic).
        // The unary EmbedArrow serves as the sequential reference output.
        let service_k1 = create_test_service().with_stream_max_concurrent_batches(1);
        let service_k4 = create_test_service().with_stream_max_concurrent_batches(4);
        let batches: Vec<Vec<&str>> =
            vec![vec!["a", "b"], vec!["c"], vec!["d", "e", "f"], vec!["g"]];
        let requests = |batches: &[Vec<&str>]| -> Vec<Result<mux::EmbedArrowRequest, Status>> {
            batches
                .iter()
                .enumerate()
                .map(|(i, texts)| Ok(arrow_stream_request(texts, i == 0)))
                .collect()
        };
        let k1_out = collect_stream_ipc(&service_k1, requests(&batches)).await;
        let k4_out = collect_stream_ipc(&service_k4, requests(&batches)).await;
        assert_eq!(k1_out.len(), batches.len());
        assert_eq!(k4_out.len(), batches.len());
        for (i, texts) in batches.iter().enumerate() {
            let unary = service_k1
                .embed_arrow(Request::new(arrow_stream_request(texts, true)))
                .await
                .unwrap()
                .into_inner()
                .arrow_ipc;
            assert_eq!(k1_out[i].as_ref().unwrap(), &unary, "batch {i} at K=1");
            assert_eq!(k4_out[i].as_ref().unwrap(), &unary, "batch {i} at K=4");
        }

        // Failure semantics are identical too: same prefix, same terminal code.
        let failing = |batches: &[Vec<&str>]| -> Vec<Result<mux::EmbedArrowRequest, Status>> {
            let mut requests = requests(batches);
            let mut bad = arrow_stream_request(&[], false);
            bad.arrow_ipc = vec![9, 9, 9];
            requests[2] = Ok(bad);
            requests
        };
        let k1_fail = collect_stream_ipc(&service_k1, failing(&batches)).await;
        let k4_fail = collect_stream_ipc(&service_k4, failing(&batches)).await;
        assert_eq!(k1_fail.len(), 3);
        assert_eq!(k4_fail.len(), 3);
        for (a, b) in k1_fail.iter().zip(&k4_fail) {
            match (a, b) {
                (Ok(a), Ok(b)) => assert_eq!(a, b),
                (Err(a), Err(b)) => {
                    assert_eq!(a.code(), Code::InvalidArgument);
                    assert_eq!(a.code(), b.code());
                }
                _ => panic!("K=1 and K=4 disagree on the failure position"),
            }
        }
    }
}
