# gRPC Multiplexer

The gRPC multiplexer provides a unified endpoint for routing embedding requests to multiple TEI instances. Instead of connecting directly to individual TEI instances, clients connect to the multiplexer which routes requests based on instance name.

## Overview

```
┌─────────┐                    ┌──────────────┐
│         │   gRPC Request     │              │
│ Client  ├───────────────────►│ Multiplexer  │
│         │   (port 9001)      │              │
└─────────┘                    └───────┬──────┘
                                       │
                        ┌──────────────┼──────────────┐
                        │              │              │
                        ▼              ▼              ▼
                   ┌─────────┐   ┌─────────┐   ┌─────────┐
                   │   TEI   │   │   TEI   │   │   TEI   │
                   │Instance │   │Instance │   │Instance │
                   │  :8081  │   │  :8082  │   │  :8083  │
                   └─────────┘   └─────────┘   └─────────┘
```

## Features

- **Unified Endpoint**: Single gRPC connection for all TEI instances
- **Dynamic Routing**: Route requests by instance name
- **Connection Pooling**: Lazy connection creation and reuse via HTTP/2
- **Full TEI API Support**: All RPC methods (embed, rerank, tokenize, etc.)
- **Streaming Support**: Bidirectional streaming for batch processing
- **Low Overhead**: Minimal performance impact (1-22% depending on workload)

## Supported RPC Methods

### Unary RPCs
- `Info` - Get model information
- `Embed` - Generate dense embeddings
- `EmbedSparse` - Generate sparse embeddings (SPLADE)
- `EmbedAll` - Generate all embedding types
- `Predict` - Single sequence classification
- `PredictPair` - Sequence pair classification
- `Rerank` - Document reranking
- `Tokenize` - Tokenize input text
- `Decode` - Decode token IDs to text

### Streaming RPCs
- `EmbedStream` - Streaming embedding generation
- `EmbedSparseStream` - Streaming sparse embeddings
- `EmbedAllStream` - Streaming all embedding types
- `PredictStream` - Streaming classification
- `PredictPairStream` - Streaming pair classification
- `RerankStream` - Streaming reranking
- `TokenizeStream` - Streaming tokenization
- `DecodeStream` - Streaming decoding

## Configuration

### Server Configuration

The multiplexer server starts automatically with TEI Manager on port 9001:

```toml
# Default configuration
[grpc]
multiplexer_port = 9001
max_parallel_streams = 1024  # Max concurrent streams per connection
```

### Environment Variables

```bash
TEI_MANAGER_GRPC_PORT=9001
TEI_MANAGER_GRPC_MAX_STREAMS=1024
```

## Usage

### Python Client Example

```python
import grpc
from tei_manager.grpc.proto.multiplexer.v1 import (
    tei_multiplexer_pb2 as mux_pb2,
    tei_multiplexer_pb2_grpc as mux_grpc,
)
from tei_manager.grpc.proto.tei.v1 import tei_pb2

# Connect to multiplexer
channel = grpc.insecure_channel('localhost:9001')
stub = mux_grpc.TeiMultiplexerStub(channel)

# Create request with routing
request = mux_pb2.EmbedRequest(
    target=mux_pb2.Target(
        instance_name="bge-small"  # Route to specific instance
    ),
    request=tei_pb2.EmbedRequest(
        inputs="Hello world",
        truncate=True,
        normalize=True,
    )
)

# Generate embeddings
response = stub.Embed(request)
print(f"Embeddings: {response.embeddings}")
```

### Streaming Example

```python
import grpc
from tei_manager.grpc.proto.multiplexer.v1 import (
    tei_multiplexer_pb2 as mux_pb2,
    tei_multiplexer_pb2_grpc as mux_grpc,
)
from tei_manager.grpc.proto.tei.v1 import tei_pb2

channel = grpc.insecure_channel('localhost:9001')
stub = mux_grpc.TeiMultiplexerStub(channel)

# Create stream of requests
def request_stream():
    texts = ["Hello", "World", "Streaming", "Example"]
    for text in texts:
        yield mux_pb2.EmbedRequest(
            target=mux_pb2.Target(instance_name="bge-small"),
            request=tei_pb2.EmbedRequest(
                inputs=text,
                truncate=True,
                normalize=True,
            )
        )

# Stream embeddings
responses = stub.EmbedStream(request_stream())
for response in responses:
    print(f"Embeddings: {response.embeddings}")
```

### Rust Client Example

```rust
use tonic::Request;
use tei_manager::grpc::proto::multiplexer::v1::{
    tei_multiplexer_client::TeiMultiplexerClient,
    EmbedRequest as MuxEmbedRequest,
    Target,
    target::Routing,
};
use tei_manager::grpc::proto::tei::v1::{
    EmbedRequest,
    TruncationDirection,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to multiplexer
    let mut client = TeiMultiplexerClient::connect("http://localhost:9001").await?;

    // Create request with routing
    let request = Request::new(MuxEmbedRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName("bge-small".to_string())),
        }),
        request: Some(EmbedRequest {
            inputs: "Hello world".to_string(),
            truncate: true,
            normalize: true,
            truncation_direction: TruncationDirection::Right as i32,
            prompt_name: None,
            dimensions: None,
        }),
    });

    // Generate embeddings
    let response = client.embed(request).await?;
    println!("Embeddings: {:?}", response.into_inner().embeddings);

    Ok(())
}
```

## Performance Benchmarks

Comprehensive benchmarks comparing direct TEI connection vs multiplexer routing across different workload scenarios.

### Test Environment
- **Model**: BAAI/bge-small-en-v1.5
- **GPU**: NVIDIA GeForce RTX 4080 SUPER (16GB)
- **Direct Connection**: `grpc://localhost:8081` (TEI instance)
- **Multiplexer**: `grpc://localhost:9001` → `bge-small` instance

### Unary RPCs (Single Request/Response)

| Text Size  | Direct (µs) | Multiplexer (µs) | Overhead (µs) | Overhead % |
|------------|-------------|------------------|---------------|------------|
| short      | 1,093.7     | 1,262.0          | 168.4         | **15.4%**  |
| medium     | 1,133.3     | 1,382.7          | 249.4         | **22.0%**  |
| long       | 1,472.4     | 1,656.8          | 184.3         | **12.5%**  |
| extra-long | 2,147.7     | 2,305.3          | 157.6         | **7.3%**   |

**Key Findings:**
- Overhead decreases with payload size (7-22%)
- Overhead is amortized over larger payloads
- Absolute overhead remains consistent (157-249µs)

### Concurrent Requests (Parallel Load)

| Concurrency | Direct (µs) | Multiplexer (µs) | Overhead (µs) | Overhead % |
|-------------|-------------|------------------|---------------|------------|
| 5           | 1,688.4     | 1,825.5          | 137.1         | **8.1%**   |
| 10          | 1,960.7     | 2,151.4          | 190.7         | **9.7%**   |
| 20          | 3,821.0     | 4,184.4          | 363.4         | **9.5%**   |

**Key Findings:**
- Excellent scaling: Overhead stays consistent at 8-10%
- No degradation with increased concurrency
- Connection pooling and HTTP/2 multiplexing work efficiently

### Streaming RPCs (Bidirectional Streaming)

| Batch Size | Direct (µs) | Multiplexer (µs) | Overhead (µs) | Overhead % |
|------------|-------------|------------------|---------------|------------|
| 5          | 1,767.7     | 2,054.0          | 286.4         | **16.2%**  |
| 10         | 1,985.3     | 2,267.9          | 282.7         | **14.2%**  |
| 20         | 3,617.9     | 3,654.7          | 36.8          | **1.0%**   |

**Key Findings:**
- Outstanding performance at higher batch sizes
- Overhead drops from 16% to **1%** as batch size increases
- Fixed overhead amortized across streaming batch
- Ideal for high-throughput batch processing

### Summary

**Overall Performance Characteristics:**

1. **Unary RPCs**: 7-22% overhead, acceptable for most use cases
2. **Concurrent Load**: 8-10% overhead with excellent scaling
3. **Streaming**: 1-16% overhead, exceptional at batch size 20+

**Overhead Breakdown:**
- Connection routing: ~50µs
- Protobuf wrapping/unwrapping: ~80-120µs
- HTTP/2 overhead: ~40-80µs

**Recommendation:**
- Use multiplexer for unified client interface and dynamic routing
- For latency-critical single requests, consider direct connection
- For batch/streaming workloads, multiplexer overhead is negligible

## Architecture

### Connection Pool

The multiplexer maintains a connection pool for backend TEI instances:

```rust
struct BackendPool {
    registry: Arc<Registry>,
    connections: Arc<DashMap<String, TeiClients>>,
}
```

**Features:**
- Lazy connection creation (on-demand)
- Lock-free concurrent access via DashMap
- Automatic connection reuse via HTTP/2
- Graceful cleanup on instance removal

### Request Flow

1. **Client Request**: Client sends gRPC request to multiplexer
2. **Target Extraction**: Extract routing target (instance name) from request
3. **Instance Lookup**: Validate instance exists and is running
4. **Connection Pool**: Get or create connection to backend instance
5. **Request Forwarding**: Forward unwrapped request to backend
6. **Response Wrapping**: Wrap backend response and return to client

### Error Handling

**Common Errors:**
- `INVALID_ARGUMENT` - Missing or invalid target
- `NOT_FOUND` - Instance not found in registry
- `UNAVAILABLE` - Instance not running or connection failed
- `UNIMPLEMENTED` - Routing strategy not supported

## Routing Strategies

### Current: Instance Name Routing

Route requests to specific instance by name:

```protobuf
message Target {
  oneof routing {
    string instance_name = 1;  // Currently supported
    uint32 index = 2;          // Future: Round-robin by index
    string model = 3;          // Future: Route by model ID
  }
}
```

### Future: Additional Routing Strategies

**Round-Robin by Index:**
```python
target = Target(index=0)  # Route to first available instance
```

**Model-Based Routing:**
```python
target = Target(model="BAAI/bge-small-en-v1.5")  # Auto-select instance
```

## Monitoring

### Metrics

The multiplexer exposes Prometheus metrics:

```
# Connection pool stats
tei_grpc_pool_connections_total{instance="bge-small"} 1
tei_grpc_pool_requests_total{instance="bge-small", method="embed"} 1234

# Request latency histogram
tei_grpc_multiplexer_request_duration_seconds{method="embed"} {...}
```

### Health Checks

The multiplexer validates instance health before routing:
- Check instance exists in registry
- Verify instance status is "running"
- Validate connection to backend

## Best Practices

### Client Configuration

**Connection Pooling:**
```python
# Reuse channel across requests
channel = grpc.insecure_channel('localhost:9001')
stub = mux_grpc.TeiMultiplexerStub(channel)

# Don't create new channel per request
for text in texts:
    response = stub.Embed(request)  # Reuses channel
```

**Keepalive Settings:**
```python
channel = grpc.insecure_channel(
    'localhost:9001',
    options=[
        ('grpc.keepalive_time_ms', 30000),
        ('grpc.keepalive_timeout_ms', 10000),
        ('grpc.keepalive_permit_without_calls', True),
    ]
)
```

### Batch Processing

For high-throughput scenarios, use streaming RPCs:

```python
# Efficient: Streaming with batch size 20+
def request_stream():
    for batch in batches:
        for text in batch:
            yield create_request(text)

responses = stub.EmbedStream(request_stream())
```

**vs**

```python
# Less efficient: Individual unary requests
for text in texts:
    response = stub.Embed(create_request(text))
```

## Troubleshooting

### Connection Refused

**Symptom:**
```
grpc._channel._InactiveRpcError: <_InactiveRpcError ...> Connection refused
```

**Solution:**
- Verify multiplexer is running: `curl http://localhost:9000/health`
- Check multiplexer port: Default is 9001
- Ensure no firewall blocking gRPC port

### Instance Not Found

**Symptom:**
```
grpc.RpcError: <_InactiveRpcError ...> Instance 'xxx' not found
```

**Solution:**
- List instances: `curl http://localhost:9000/instances`
- Verify instance name matches exactly
- Check instance exists and is running

### Instance Not Running

**Symptom:**
```
grpc.RpcError: <_InactiveRpcError ...> Instance 'xxx' is not running (status: stopped)
```

**Solution:**
- Start instance: `curl -X POST http://localhost:9000/instances/xxx/start`
- Check instance health: `curl http://localhost:9000/instances/xxx`

## Development

### Running Benchmarks

```bash
# Full benchmark suite
./bench-multiplexer.sh

# Criterion benchmarks only
cargo bench --bench multiplexer_overhead

# Specific benchmark group
cargo bench --bench multiplexer_overhead -- embedding_overhead
cargo bench --bench multiplexer_overhead -- concurrent_requests
cargo bench --bench multiplexer_overhead -- streaming_requests
```

### Protobuf Compilation

Protocol buffers are automatically compiled via `build.rs`:

```bash
# Rebuild protobufs
cargo clean
cargo build

# Generated code location
target/debug/build/tei-manager-*/out/
```

### Adding New RPC Methods

1. Update `proto/multiplexer/v1/multiplexer.proto` with new method
2. Implement method in `src/grpc/multiplexer.rs`
3. Add tests in `src/grpc/multiplexer.rs`
4. Update benchmarks in `benches/multiplexer_overhead.rs`

## References

- [gRPC Documentation](https://grpc.io/docs/)
- [Tonic (Rust gRPC)](https://github.com/hyperium/tonic)
- [Protocol Buffers](https://protobuf.dev/)
- [TEI gRPC API](https://github.com/huggingface/text-embeddings-inference)
