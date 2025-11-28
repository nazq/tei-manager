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

### 1. Add Dependencies

```toml
# Cargo.toml
[dependencies]
tonic = "0.12"
prost = "0.13"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }

[build-dependencies]
tonic-build = "0.12"
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
    tonic_build::configure()
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
            normalize: true,
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
            normalize: true,
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
                normalize: true,
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
use arrow::array::{ArrayRef, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    EmbedArrowRequest, Target, target::Routing,
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

    // Serialize to Arrow IPC with LZ4 compression
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

**Arrow dependencies:**

```toml
[dependencies]
arrow = { version = "53", features = ["ipc_compression"] }
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
            eprintln!("Instance not found: {}", status.message());
        }
        tonic::Code::Unavailable => {
            eprintln!("Instance not running: {}", status.message());
        }
        _ => {
            eprintln!("Error: {} - {}", status.code(), status.message());
        }
    }
}
```

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
        string instance_name = 1;  // Route by instance name (recommended)
        string model_id = 2;       // Route by model ID (future)
        uint32 instance_index = 3; // Route by index (future)
    }
}
```

Currently only `instance_name` routing is supported. Model-based and index-based routing are planned for future releases.

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
