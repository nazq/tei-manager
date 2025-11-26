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
- `EmbedArrow` - Batch dense embeddings via Arrow IPC
- `EmbedSparseArrow` - Batch sparse embeddings via Arrow IPC
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

### grpcurl Examples

```bash
# Generate embeddings via gRPC multiplexer
grpcurl -plaintext -d '{
  "target": {"instance_name": "bge-small"},
  "request": {"inputs": "Hello world", "truncate": true, "normalize": true}
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/Embed

# Get model info
grpcurl -plaintext -d '{
  "target": {"instance_name": "bge-small"}
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/Info

# Generate sparse embeddings (SPLADE)
grpcurl -plaintext -d '{
  "target": {"instance_name": "splade"},
  "request": {"inputs": "Information retrieval"}
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/EmbedSparse

# Batch dense embeddings via Arrow IPC
grpcurl -plaintext -d '{
  "target": {"instance_name": "bge-small"},
  "arrow_ipc": "<base64-encoded-arrow-ipc>",
  "truncate": true,
  "normalize": true
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/EmbedArrow

# Batch sparse embeddings via Arrow IPC (SPLADE models)
grpcurl -plaintext -d '{
  "target": {"instance_name": "splade"},
  "arrow_ipc": "<base64-encoded-arrow-ipc>",
  "truncate": true
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/EmbedSparseArrow

# List available services
grpcurl -plaintext localhost:9001 list
```

### Rust Client Example

For a complete Rust client implementation, see the built-in benchmark client at `src/bin/bench-client.rs`. It demonstrates:

- Connecting to the gRPC multiplexer with/without TLS
- Creating Arrow IPC batches with LZ4 compression
- Sending `EmbedArrow` requests and parsing responses
- Concurrent request handling with Tokio

```bash
# Build and run the benchmark client
cargo build --release --bin bench-client

# Standard mode: concurrent single-text requests
bench-client -e http://localhost:9001 -i bge-small \
  --mode standard --num-texts 10000 --batch-size 100

# Arrow mode: batched Arrow IPC requests (recommended for throughput)
bench-client -e http://localhost:9001 -i bge-small \
  --mode arrow --num-texts 100000 --batch-size 1000
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

### Arrow IPC Batch Processing

Arrow IPC provides efficient batch embedding via a single request:

| Batch Size | Arrow (ms) | Streaming (ms) | Arrow Advantage |
|------------|------------|----------------|-----------------|
| 1          | ~1.0       | ~1.0           | Similar         |
| 10         | ~2.1       | ~2.1           | Similar         |
| 50         | ~5.0       | ~5.5           | ~10% faster     |
| 100        | ~9.0       | ~11.0          | ~18% faster     |

**Key Findings:**
- Arrow batch scales better for larger batches
- Single request reduces connection overhead
- LZ4 compression reduces network payload

### Summary

**Overall Performance Characteristics:**

1. **Unary RPCs**: 8-13% overhead, acceptable for most use cases
2. **Concurrent Load**: 8-12% overhead with excellent scaling
3. **Streaming**: 8-10% overhead, consistent across batch sizes
4. **Arrow Batch**: Best for large batches (50+), single-request efficiency

**Recommendation:**
- Use multiplexer for unified client interface and dynamic routing
- For batch processing, prefer Arrow IPC over streaming
- Multiplexer overhead is consistent and predictable (~8-13%)

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

**Connection Reuse:**
- Reuse gRPC channels across requests (HTTP/2 multiplexing)
- Don't create new connections per request

**Keepalive Settings:**
- Configure keepalive for long-running connections
- Set `grpc.keepalive_time_ms` to 30000ms
- Set `grpc.keepalive_timeout_ms` to 10000ms

### Batch Processing

For high-throughput scenarios, use the `EmbedArrow` or `EmbedSparseArrow` endpoints:

```bash
# Arrow mode: batch thousands of texts in a single request
bench-client -e http://localhost:9001 -i bge-small \
  --mode arrow --num-texts 100000 --batch-size 1000
```

**Benefits of Arrow IPC:**
- Process thousands of texts in a single request
- LZ4 compression reduces network overhead
- Efficient memory layout for batch processing
- Dense (`EmbedArrow`): Returns `FixedSizeList<Float32>` for zero-copy access
- Sparse (`EmbedSparseArrow`): Returns `List<Struct<index:u32, value:f32>>` for variable-length sparse vectors

## Troubleshooting

### Connection Refused

**Symptom:**
```
Connection refused / transport: Error while dialing
```

**Solution:**
- Verify multiplexer is running: `curl http://localhost:9000/health`
- Check multiplexer port: Default is 9001
- Ensure no firewall blocking gRPC port

### Instance Not Found

**Symptom:**
```
NOT_FOUND: Instance 'xxx' not found
```

**Solution:**
- List instances: `curl http://localhost:9000/instances`
- Verify instance name matches exactly
- Check instance exists and is running

### Instance Not Running

**Symptom:**
```
UNAVAILABLE: Instance 'xxx' is not running (status: stopped)
```

**Solution:**
- Start instance: `curl -X POST http://localhost:9000/instances/xxx/start`
- Check instance health: `curl http://localhost:9000/instances/xxx`

## Development

### Running Benchmarks

Benchmarks require a running TEI instance. Use the provided just targets to manage the benchmark environment:

```bash
# Terminal 1: Start the benchmark environment
just bench-start

# This will:
# 1. Build the release binary
# 2. Start tei-manager on ports 9000 (REST) and 9001 (gRPC)
# 3. Create a "bench-instance" on port 8081
# 4. Wait for the instance to be ready

# Terminal 2: Run benchmarks
just bench

# Or run specific benchmark groups
cargo bench --bench multiplexer_overhead -- embedding_overhead
cargo bench --bench multiplexer_overhead -- concurrent_requests
cargo bench --bench multiplexer_overhead -- streaming_requests
cargo bench --bench multiplexer_overhead -- arrow_batch

# Check environment status
just bench-status

# Stop the benchmark environment (Ctrl+C in Terminal 1, or:)
just bench-stop
```

### Protobuf Compilation

Protocol buffers are automatically compiled via `build.rs`:

```bash
# Rebuild protobufs
just clean && just build

# Generated code location
target/debug/build/tei-manager-*/out/
```

### Adding New RPC Methods

1. Update `proto/tei_multiplexer/v1/multiplexer.proto` with new method
2. Implement method in `src/grpc/multiplexer.rs`
3. Add tests in `src/grpc/multiplexer.rs`
4. Update benchmarks in `benches/multiplexer_overhead.rs`

## References

- [gRPC Documentation](https://grpc.io/docs/)
- [Tonic (Rust gRPC)](https://github.com/hyperium/tonic)
- [Protocol Buffers](https://protobuf.dev/)
- [TEI gRPC API](https://github.com/huggingface/text-embeddings-inference)
