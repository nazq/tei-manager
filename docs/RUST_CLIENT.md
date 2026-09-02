# Rust Client Integration

This guide shows how to build a Rust gRPC client for TEI Manager's multiplexer using tonic.

## Proto Structure

TEI Manager uses a nested proto structure:

```
proto/
├── tei/v1/tei.proto                     # Upstream TEI types (vendored)
└── tei_multiplexer/v1/multiplexer.proto # Multiplexer service (imports tei.proto)
```

The multiplexer proto wraps TEI's types with a `Target` field for routing:

```protobuf
// multiplexer.proto
import "tei/v1/tei.proto";

message EmbedRequest {
    Target target = 1;              // Routing info
    tei.v1.EmbedRequest request = 2; // Nested TEI request
}
```

## Quick Start

The examples assume a running instance. Create one via the REST API (port 9000):

```bash
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "bge-small",
    "model_id": "BAAI/bge-small-en-v1.5",
    "max_batch_tokens": "auto",
    "log_level": "warn"
  }'
```

`max_batch_tokens` takes a number or `"auto"` (derived from the target GPU's free VRAM at creation; the resolved value is returned as `max_batch_tokens` in the instance JSON). `log_level` sets the TEI child's `RUST_LOG` filter (default `warn`). If an instance fails, `GET /instances/<name>` includes `last_error` with the reason.

### 1. Add Dependencies

```toml
# Cargo.toml
[dependencies]
tonic = "0.14"
tonic-prost = "0.14"
prost = "0.14"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
tokio-stream = "0.1"  # For streaming RPCs

[build-dependencies]
tonic-prost-build = "0.14"
```

### 2. Copy Proto Files

Copy both proto directories from this repository:

```bash
mkdir -p proto/tei/v1 proto/tei_multiplexer/v1

# Copy from tei-manager repo (or download from GitHub)
cp tei-manager/proto/tei/v1/tei.proto proto/tei/v1/
cp tei-manager/proto/tei_multiplexer/v1/multiplexer.proto proto/tei_multiplexer/v1/
```

### 3. Create build.rs

```rust
// build.rs
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(false)  // Client only
        .compile_protos(
            &["proto/tei_multiplexer/v1/multiplexer.proto"],
            &["proto"],  // Include path for imports
        )?;
    Ok(())
}
```

### 4. Include Generated Code

```rust
// src/proto.rs
pub mod tei {
    pub mod v1 {
        tonic::include_proto!("tei.v1");
    }
}

pub mod multiplexer {
    pub mod v1 {
        tonic::include_proto!("tei_multiplexer.v1");
    }
}
```

### 5. Use the Client

```rust
// src/main.rs
mod proto;

use proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    EmbedRequest, Target, target::Routing,
};
use proto::tei::v1 as tei;
use tonic::transport::Channel;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to multiplexer
    let channel = Channel::from_static("http://localhost:9001")
        .connect()
        .await?;

    let mut client = TeiMultiplexerClient::new(channel);

    // Build request with Target for routing
    let request = EmbedRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName("bge-small".to_string())),
        }),
        request: Some(tei::EmbedRequest {
            inputs: "Hello, world!".to_string(),
            truncate: true,
            normalize: Some(true),
            truncation_direction: 0,
            prompt_name: None,
            dimensions: None,
        }),
    };

    let response = client.embed(request).await?;
    let embeddings = response.into_inner().embeddings;

    println!("Embedding dimension: {}", embeddings.len());
    Ok(())
}
```

## Complete Examples

### Dense Embeddings

```rust
use proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    EmbedRequest, Target, target::Routing,
};
use proto::tei::v1 as tei;

async fn embed_text(
    client: &mut TeiMultiplexerClient<Channel>,
    instance: &str,
    text: &str,
) -> Result<Vec<f32>, tonic::Status> {
    let request = EmbedRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName(instance.to_string())),
        }),
        request: Some(tei::EmbedRequest {
            inputs: text.to_string(),
            truncate: true,
            normalize: Some(true),
            truncation_direction: 0,  // Right truncation
            prompt_name: None,
            dimensions: None,
        }),
    };

    let response = client.embed(request).await?;
    Ok(response.into_inner().embeddings)
}
```

### Sparse Embeddings (SPLADE)

```rust
use proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    EmbedSparseRequest, Target, target::Routing,
};
use proto::tei::v1 as tei;

async fn embed_sparse(
    client: &mut TeiMultiplexerClient<Channel>,
    instance: &str,
    text: &str,
) -> Result<Vec<(u32, f32)>, tonic::Status> {
    let request = EmbedSparseRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName(instance.to_string())),
        }),
        request: Some(tei::EmbedSparseRequest {
            inputs: text.to_string(),
            truncate: true,
            truncation_direction: 0,
            prompt_name: None,
        }),
    };

    let response = client.embed_sparse(request).await?;
    let sparse = response
        .into_inner()
        .sparse_embeddings
        .into_iter()
        .map(|sv| (sv.index, sv.value))
        .collect();

    Ok(sparse)
}
```

### Reranking

```rust
use proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    RerankRequest, Target, target::Routing,
};
use proto::tei::v1 as tei;

async fn rerank(
    client: &mut TeiMultiplexerClient<Channel>,
    instance: &str,
    query: &str,
    documents: Vec<String>,
) -> Result<Vec<(usize, f32)>, tonic::Status> {
    let request = RerankRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName(instance.to_string())),
        }),
        request: Some(tei::RerankRequest {
            query: query.to_string(),
            texts: documents,
            truncate: true,
            raw_scores: false,
            return_text: false,
            truncation_direction: 0,
        }),
    };

    let response = client.rerank(request).await?;
    let ranks = response
        .into_inner()
        .ranks
        .into_iter()
        .map(|r| (r.index as usize, r.score))
        .collect();

    Ok(ranks)
}
```

### Get Model Info

```rust
use proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    InfoRequest, Target, target::Routing,
};
use proto::tei::v1::InfoResponse;

async fn get_info(
    client: &mut TeiMultiplexerClient<Channel>,
    instance: &str,
) -> Result<InfoResponse, tonic::Status> {
    let request = InfoRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName(instance.to_string())),
        }),
    };

    let response = client.info(request).await?;
    Ok(response.into_inner())
}
```

### Streaming Embeddings

```rust
use proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    EmbedRequest, Target, target::Routing,
};
use proto::tei::v1 as tei;
use tokio_stream::StreamExt;

async fn embed_stream(
    client: &mut TeiMultiplexerClient<Channel>,
    instance: &str,
    texts: Vec<String>,
) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
    let instance = instance.to_string();

    // Create request stream
    let request_stream = tokio_stream::iter(texts.into_iter().map(move |text| {
        EmbedRequest {
            target: Some(Target {
                routing: Some(Routing::InstanceName(instance.clone())),
            }),
            request: Some(tei::EmbedRequest {
                inputs: text,
                truncate: true,
                normalize: Some(true),
                truncation_direction: 0,
                prompt_name: None,
                dimensions: None,
            }),
        }
    }));

    // Send stream and collect responses
    let mut response_stream = client.embed_stream(request_stream).await?.into_inner();

    let mut embeddings = Vec::new();
    while let Some(response) = response_stream.next().await {
        embeddings.push(response?.embeddings);
    }

    Ok(embeddings)
}
```

## Arrow Batch Embeddings

For high-throughput scenarios, use Arrow IPC batch embedding:

```rust
use arrow::array::{Array, ArrayRef, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    ArrowCompression, EmbedArrowRequest, OutputDtype, Target, target::Routing,
};
use std::io::Cursor;
use std::sync::Arc;

async fn embed_arrow_batch(
    client: &mut TeiMultiplexerClient<Channel>,
    instance: &str,
    texts: Vec<String>,
) -> Result<RecordBatch, Box<dyn std::error::Error>> {
    // Create Arrow RecordBatch with text column
    let text_array = StringArray::from(texts);
    let schema = Arc::new(Schema::new(vec![
        Field::new("text", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(text_array) as ArrayRef],
    )?;

    // Serialize to Arrow IPC. LZ4 on the request side is worthwhile for
    // text-heavy payloads.
    let mut arrow_ipc = Vec::new();
    {
        use arrow::ipc::writer::IpcWriteOptions;
        use arrow::ipc::CompressionType;

        let write_options = IpcWriteOptions::default()
            .try_with_compression(Some(CompressionType::LZ4_FRAME))?;

        let mut writer = StreamWriter::try_new_with_options(
            &mut arrow_ipc,
            &schema,
            write_options,
        )?;
        writer.write(&batch)?;
        writer.finish()?;
    }

    // Send request
    let request = EmbedArrowRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName(instance.to_string())),
        }),
        arrow_ipc,
        truncate: true,
        normalize: true,
        noop: false,
        truncation_direction: 0,  // TruncationDirection: 0 = right, 1 = left
        prompt_name: None,        // Prompt/instruction prefix configured on the model
        dimensions: None,         // Matryoshka truncation of the output vector
        // Response IPC compression. NONE is the default and the fastest
        // choice — dense vectors are effectively incompressible; opt in to
        // LZ4 (ArrowCompression::Lz4) for text-heavy payloads.
        compression: ArrowCompression::None as i32,
        // Element type of the embeddings column. Unspecified uses the
        // server's configured default (`arrow_output_dtype`, f32 unless
        // changed). F16 halves the payload — see below.
        output_dtype: OutputDtype::Unspecified as i32,
    };

    // Increase message size limit for large batches
    let mut client = client.clone();
    client = client
        .max_decoding_message_size(100 * 1024 * 1024)
        .max_encoding_message_size(100 * 1024 * 1024);

    let response = client.embed_arrow(request).await?;
    let response_ipc = response.into_inner().arrow_ipc;

    // Deserialize response
    let cursor = Cursor::new(response_ipc);
    let mut reader = StreamReader::try_new(cursor, None)?;
    let result_batch = reader.next().ok_or("No batch in response")??;

    Ok(result_batch)
}
```

### Response Shape: Check the Error Column

The response RecordBatch has **exactly one row per input row, in input order**, with two columns:

- Column 0 `embeddings` — `FixedSizeList<Float32|Float16>[dim]`, nullable: **null when that row failed**
- Column 1 `error` — `Utf8`, nullable: the per-row failure reason, null on success

The RPC succeeds even when individual rows fail (empty input, too long without `truncate`, null text), so don't assume every row has a vector — check the error column and skip-and-record:

```rust
let errors = result_batch
    .column(1)
    .as_any()
    .downcast_ref::<StringArray>()
    .ok_or("missing error column")?;

for row in 0..result_batch.num_rows() {
    if errors.is_valid(row) {
        eprintln!("row {} failed: {}", row, errors.value(row));
        continue;  // embeddings is null for this row
    }
    // embeddings holds a valid vector for this row
}
```

One bad document no longer fails the batch. Backend failures (dead instance, timeout) still fail the whole call with a gRPC status.

### Output Dtype (f16)

Requesting `OutputDtype::F16` halves the payload; widen to `f32` on the client. The conversion is lossy — validated to leave top-k ordering unchanged for normalized-cosine retrieval, but measure on your own models and data before relying on it. The server default is f32 unless `arrow_output_dtype = "f16"` is configured.

### Sparse Batches

`EmbedSparseArrow` works the same way: `EmbedSparseArrowRequest` takes `target`, `arrow_ipc`, `truncate`, `noop`, `truncation_direction`, `prompt_name` and `compression` (no `normalize`, `dimensions` or `output_dtype`). The response has `sparse_embeddings` (`List<Struct<index:u32, value:f32>>`, nullable) plus the same nullable `error` column, one row per input row.

**Arrow dependencies:**

```toml
[dependencies]
arrow = { version = "59", features = ["ipc_compression"] }
```

## Connection Options

### With Keepalive

```rust
use std::time::Duration;
use tonic::transport::Channel;

let channel = Channel::from_static("http://localhost:9001")
    .tcp_keepalive(Some(Duration::from_secs(60)))
    .http2_keep_alive_interval(Duration::from_secs(30))
    .keep_alive_timeout(Duration::from_secs(10))
    .connect_timeout(Duration::from_secs(5))
    .connect()
    .await?;
```

### With mTLS

```rust
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

let cert = tokio::fs::read("client.pem").await?;
let key = tokio::fs::read("client-key.pem").await?;
let ca = tokio::fs::read("ca.pem").await?;

let tls_config = ClientTlsConfig::new()
    .identity(Identity::from_pem(cert, key))
    .ca_certificate(Certificate::from_pem(ca));

let channel = Channel::from_static("https://localhost:9001")
    .tls_config(tls_config)?
    .connect()
    .await?;
```

### Request Timeouts

The multiplexer applies its own timeout to forwarded requests — including the Arrow RPCs — controlled by `grpc_request_timeout_secs` in the server config (default: 30). For long-running batches, set a per-call deadline on the client; tonic sends it as the standard `grpc-timeout` metadata:

```rust
use std::time::Duration;

let mut request = tonic::Request::new(embed_request);
request.set_timeout(Duration::from_secs(120));
let response = client.embed_arrow(request).await?;
```

## Tracing (OpenTelemetry)

The multiplexer honours W3C `traceparent`/`tracestate` on gRPC metadata. Inject your current OTel context with a tonic interceptor and the manager's spans (`tei.embed_arrow`, with a `tei.embed_stream` child per backend stream) appear as children in the same trace:

```rust
use opentelemetry::global;
use opentelemetry::propagation::Injector;
use tonic::metadata::{MetadataKey, MetadataMap};
use tonic::service::Interceptor;
use tracing_opentelemetry::OpenTelemetrySpanExt;

struct MetadataInjector<'a>(&'a mut MetadataMap);

impl Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(key), Ok(value)) = (MetadataKey::from_bytes(key.as_bytes()), value.parse()) {
            self.0.insert(key, value);
        }
    }
}

#[derive(Clone)]
struct TraceContextInterceptor;

impl Interceptor for TraceContextInterceptor {
    fn call(&mut self, mut request: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let context = tracing::Span::current().context();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut MetadataInjector(request.metadata_mut()));
        });
        Ok(request)
    }
}

let mut client = TeiMultiplexerClient::with_interceptor(channel, TraceContextInterceptor);
```

Requires `opentelemetry = "0.32"` and `tracing-opentelemetry = "0.33"`, with the W3C `TraceContextPropagator` installed as the global propagator on your side (`global::set_text_map_propagator(TraceContextPropagator::new())`).

## Error Handling

The multiplexer returns standard gRPC status codes:

```rust
use tonic::Status;

match client.embed(request).await {
    Ok(response) => {
        let embeddings = response.into_inner().embeddings;
        // Process embeddings...
    }
    Err(status) => match status.code() {
        tonic::Code::InvalidArgument => {
            eprintln!("Invalid request: {}", status.message());
        }
        tonic::Code::NotFound => {
            // Unknown instance name, or model_id routing with zero
            // running instances of that model (the message names it)
            eprintln!("No target: {}", status.message());
        }
        tonic::Code::Unavailable => {
            eprintln!("Instance not running: {}", status.message());
        }
        tonic::Code::DeadlineExceeded => {
            // Server-side grpc_request_timeout_secs (default 30s) or
            // your own per-call deadline
            eprintln!("Timed out: {}", status.message());
        }
        _ => {
            eprintln!("Error: {} - {}", status.code(), status.message());
        }
    }
}
```

Note that for `EmbedArrow`/`EmbedSparseArrow`, per-row failures do **not** produce a gRPC error — the call succeeds and the failures land in the response's `error` column (see [Arrow Batch Embeddings](#arrow-batch-embeddings)).

## Reference Implementation

For a complete working example, see the built-in benchmark client:

- **Source**: `src/bin/bench-client.rs`
- **Features**: Standard embedding, Arrow batching, mTLS, concurrent requests

```bash
# Run the benchmark client
cargo run --release --bin bench-client -- \
    --endpoint http://localhost:9001 \
    --instance bge-small \
    --mode standard \
    --num-texts 1000 \
    --batch-size 100
```

## Proto Reference

### Target (Routing)

```protobuf
message Target {
    oneof routing {
        string instance_name = 1;  // Route by instance name (e.g., "bge-small")
        string model_id = 2;        // Route to a running instance serving this model
        uint32 instance_index = 3;  // Route by instance index (future)
    }
}
```

`instance_name` routes to that specific instance. `model_id` routes to any *running* instance serving that model — round-robin across matches, with the whole RPC (including a full stream) pinned to one instance, so a batch is never split. Zero running matches returns `NotFound` naming the model. On a multi-GPU box, create one instance per GPU with the same `model_id` and simply target the model:

```rust
Target {
    routing: Some(Routing::ModelId("BAAI/bge-m3".to_string())),
}
```

`instance_index` routing is not yet implemented and returns `Unimplemented`.

### Available RPCs

| RPC | Request Type | Response Type | Description |
|-----|--------------|---------------|-------------|
| `Info` | `InfoRequest` | `tei.v1.InfoResponse` | Get model info |
| `Embed` | `EmbedRequest` | `tei.v1.EmbedResponse` | Dense embeddings |
| `EmbedStream` | `stream EmbedRequest` | `stream tei.v1.EmbedResponse` | Streaming dense |
| `EmbedSparse` | `EmbedSparseRequest` | `tei.v1.EmbedSparseResponse` | Sparse embeddings |
| `EmbedSparseStream` | `stream EmbedSparseRequest` | `stream tei.v1.EmbedSparseResponse` | Streaming sparse |
| `EmbedAll` | `EmbedAllRequest` | `tei.v1.EmbedAllResponse` | Token-level embeddings |
| `EmbedAllStream` | `stream EmbedAllRequest` | `stream tei.v1.EmbedAllResponse` | Streaming token-level |
| `EmbedArrow` | `EmbedArrowRequest` | `EmbedArrowResponse` | Arrow batch dense |
| `EmbedSparseArrow` | `EmbedSparseArrowRequest` | `EmbedSparseArrowResponse` | Arrow batch sparse |
| `Predict` | `PredictRequest` | `tei.v1.PredictResponse` | Classification |
| `PredictPair` | `PredictPairRequest` | `tei.v1.PredictResponse` | Pair classification |
| `PredictStream` | `stream PredictRequest` | `stream tei.v1.PredictResponse` | Streaming classification |
| `PredictPairStream` | `stream PredictPairRequest` | `stream tei.v1.PredictResponse` | Streaming pair classification |
| `Rerank` | `RerankRequest` | `tei.v1.RerankResponse` | Document reranking |
| `RerankStream` | `stream RerankStreamRequest` | `tei.v1.RerankResponse` | Streaming reranking |
| `Tokenize` | `EncodeRequest` | `tei.v1.EncodeResponse` | Tokenization |
| `TokenizeStream` | `stream EncodeRequest` | `stream tei.v1.EncodeResponse` | Streaming tokenization |
| `Decode` | `DecodeRequest` | `tei.v1.DecodeResponse` | Token decoding |
| `DecodeStream` | `stream DecodeRequest` | `stream tei.v1.DecodeResponse` | Streaming decoding |
