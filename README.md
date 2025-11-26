# TEI Manager

[![Rust](https://img.shields.io/badge/rust-1.91+-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![codecov](https://codecov.io/gh/nazq/tei-manager/branch/main/graph/badge.svg)](https://codecov.io/gh/nazq/tei-manager)
[![Docker](https://img.shields.io/badge/docker-ready-brightgreen.svg)](Dockerfile)
[![TEI](https://img.shields.io/badge/TEI-1.8.3-purple.svg)](https://github.com/huggingface/text-embeddings-inference)

Dynamic multi-instance manager for [HuggingFace Text Embeddings Inference](https://github.com/huggingface/text-embeddings-inference) (TEI). Run multiple embedding models simultaneously with intelligent resource management, health monitoring, and automatic recovery.

---

## Who Is This For?

TEI Manager is designed for teams running **multiple embedding models on a single GPU host** who want:

- **Unified API** - One gRPC endpoint to route requests to any model
- **Simple operations** - REST API for instance lifecycle, no orchestrator required
- **Production basics** - Health checks, auto-restart, metrics, state persistence

**Not a fit if you need:** Multi-node clustering, request queuing, per-tenant quotas, or Kubernetes-native autoscaling. For those, consider [Ray Serve](https://docs.ray.io/en/latest/serve/), [vLLM](https://vllm.ai), or [KServe](https://kserve.github.io/).

---

## Architecture

```mermaid
flowchart LR
    subgraph Clients
        C1[REST Client]
        C2[gRPC Client]
    end

    subgraph TEI Manager
        API[REST API<br/>:9000]
        MUX[gRPC Multiplexer<br/>:9001]
        HM[Health Monitor]
        ST[State Persistence]
    end

    subgraph TEI Instances
        T1[bge-small<br/>GPU 0 · :8080]
        T2[bge-large<br/>GPU 1 · :8081]
        T3[splade<br/>GPU 0 · :8082]
    end

    C1 --> API
    C2 --> MUX
    API --> T1 & T2 & T3
    MUX --> T1 & T2 & T3
    HM --> T1 & T2 & T3
    ST -.-> API
```

**Request flow:**
1. Clients send embedding requests to the gRPC Multiplexer (port 9001)
2. Multiplexer routes to the target instance based on `instance_name`
3. TEI instance processes the request on its assigned GPU
4. Response returns through the multiplexer to the client

**Management flow:**
- REST API (port 9000) handles instance lifecycle (create/start/stop/delete)
- Health Monitor checks each instance periodically, auto-restarts on failure
- State Persistence saves instance configs to disk for crash recovery

---

## Features

- **Dynamic Instance Management** - Create, start, stop, restart, and delete TEI instances via REST API
- **Multi-GPU Support** - Pin instances to specific GPUs or share across all available GPUs
- **gRPC Multiplexer** - Unified streaming gRPC endpoint for routing requests to multiple instances
- **Arrow Batch Embeddings** - High-throughput batch embedding via Arrow IPC with LZ4 compression
- **Rust Benchmark Client** - Built-in gRPC client for benchmarking and integration examples
- **State Persistence** - Automatic state saving with atomic writes and crash recovery
- **Health Monitoring** - Continuous health checks with configurable auto-restart on failure
- **Prometheus Metrics** - Built-in metrics export for monitoring instance lifecycle and operations
- **mTLS Authentication** - Optional mutual TLS for secure gRPC connections

---

## Docker Images

TEI Manager images are built on the [TEI gRPC base images](https://github.com/huggingface/text-embeddings-inference?tab=readme-ov-file#docker-images), which provide GPU-optimized kernels for embedding inference.

**Tag format:** `{manager_version}-tei-{tei_version}[-{arch}]`

| Tag | Base Image | GPU Support |
|-----|------------|-------------|
| `0.3.0-tei-1.8.3` | `..text-embeddings-inference:1.8.3-grpc` | Multi-arch (auto-detect) |
| `0.3.0-tei-1.8.3-ada` | `..text-embeddings-inference:89-1.8.3-grpc` | Ada (RTX 40xx, L4, L40, L40S) |
| `0.3.0-tei-1.8.3-hopper` | `..text-embeddings-inference:hopper-1.8-grpc` | Hopper (H100, H200) |

> **Note:** Only gRPC-enabled base images are supported. CPU-only and non-gRPC variants are not available.

---

## Quick Start

### Using Docker

```bash
# Pull the image for your GPU architecture
docker pull ghcr.io/nazq/tei-manager:0.3.0-tei-1.8.3        # Multi-arch (auto-detect)
docker pull ghcr.io/nazq/tei-manager:0.3.0-tei-1.8.3-ada    # Ada (RTX 40xx, L4, L40, L40S)
docker pull ghcr.io/nazq/tei-manager:0.3.0-tei-1.8.3-hopper # Hopper (H100, H200)

# Run with GPU support
docker run -d --gpus all \
  --name tei-manager \
  -p 9000:9000 \
  -p 9001:9001 \
  -p 8080-8089:8080-8089 \
  ghcr.io/nazq/tei-manager:0.3.0-tei-1.8.3

# Create an embedding instance
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{"name": "bge-small", "model_id": "BAAI/bge-small-en-v1.5"}'

# Wait for instance to be ready (~30s for model download)
curl http://localhost:9000/instances/bge-small

# Generate embeddings via REST (direct to TEI)
curl -X POST http://localhost:8080/embed \
  -H "Content-Type: application/json" \
  -d '{"inputs": "Hello world"}'
```

### Using gRPC with grpcurl

```bash
# Generate embeddings via gRPC multiplexer
grpcurl -plaintext -d '{
  "target": {"instance_name": "bge-small"},
  "request": {"inputs": "Hello world", "truncate": true, "normalize": true}
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/Embed

# Get instance info
grpcurl -plaintext -d '{
  "target": {"instance_name": "bge-small"}
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/Info

# List available services
grpcurl -plaintext localhost:9001 list
```

---

## gRPC API

The gRPC multiplexer provides a unified endpoint for routing embedding requests to any managed instance.

### Available Methods

| Method | Description |
|--------|-------------|
| `Embed` | Generate dense embeddings for a single text |
| `EmbedStream` | Streaming dense embeddings |
| `EmbedSparse` | Generate sparse embeddings (SPLADE) |
| `EmbedArrow` | **High-throughput batch dense embedding via Arrow IPC** |
| `EmbedSparseArrow` | **High-throughput batch sparse embedding via Arrow IPC** |
| `Rerank` | Rerank documents by relevance |
| `Tokenize` | Tokenize text |
| `Info` | Get model information |

### Arrow Batch Embeddings

The `EmbedArrow` and `EmbedSparseArrow` endpoints enable high-throughput batch processing using Apache Arrow IPC format with LZ4 compression:

```bash
# Dense embeddings via Arrow IPC
grpcurl -plaintext -d '{
  "target": {"instance_name": "bge-small"},
  "arrow_ipc": "<base64-encoded-arrow-ipc>",
  "truncate": true,
  "normalize": true
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/EmbedArrow

# Sparse embeddings via Arrow IPC (SPLADE models)
grpcurl -plaintext -d '{
  "target": {"instance_name": "splade"},
  "arrow_ipc": "<base64-encoded-arrow-ipc>",
  "truncate": true
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/EmbedSparseArrow
```

**Benefits:**
- Process thousands of texts in a single request
- LZ4 compression reduces network overhead
- Efficient memory layout for batch processing
- Dense: Returns embeddings as Arrow `FixedSizeList<Float32>` for zero-copy access
- Sparse: Returns embeddings as Arrow `List<Struct<index:u32, value:f32>>` for variable-length sparse vectors

---

## Rust Benchmark Client

TEI Manager includes a built-in Rust benchmark client for testing throughput and latency. This also serves as a complete example for integrating with the gRPC API from Rust.

### Installation

```bash
# Build from source
cargo build --release --bin bench-client

# Or run directly
cargo run --release --bin bench-client -- --help
```

### Usage

```bash
# Standard mode: concurrent single-text requests
bench-client -e http://localhost:9001 -i bge-small \
  --mode standard --num-texts 10000 --batch-size 100

# Arrow mode: batched Arrow IPC requests (recommended for throughput)
bench-client -e http://localhost:9001 -i bge-small \
  --mode arrow --num-texts 100000 --batch-size 1000

# With mTLS
bench-client -e https://localhost:9001 -i bge-small \
  --cert client.pem --key client-key.pem --ca ca.pem \
  --mode arrow --num-texts 100000 --batch-size 1000
```

### Example Output

```json
{
  "mode": "arrow",
  "instance_name": "bge-small",
  "num_texts": 100000,
  "batch_size": 1000,
  "num_requests": 100,
  "total_duration_secs": 12.34,
  "throughput_per_sec": 8103.72,
  "successful": 100000,
  "failed": 0
}
```

### Using as a Rust Library Example

The bench-client source (`src/bin/bench-client.rs`) demonstrates:
- Connecting to the gRPC multiplexer with/without TLS
- Creating Arrow IPC batches with LZ4 compression
- Sending `EmbedArrow` requests and parsing responses
- Concurrent request handling with Tokio

---

## REST API

### Endpoints

| Method | Endpoint | Description | Success | Error Codes |
|--------|----------|-------------|---------|-------------|
| `GET` | `/health` | Health check | 200 | - |
| `GET` | `/metrics` | Prometheus metrics | 200 | - |
| `GET` | `/instances` | List all instances | 200 | - |
| `GET` | `/instances/{name}` | Get instance details | 200 | 404 `INSTANCE_NOT_FOUND` |
| `POST` | `/instances` | Create new instance | 201 | 409 `INSTANCE_EXISTS`, 422 `PORT_CONFLICT` |
| `DELETE` | `/instances/{name}` | Delete instance | 200 | 404 `INSTANCE_NOT_FOUND` |
| `POST` | `/instances/{name}/start` | Start instance | 200 | 404, 409 `ALREADY_RUNNING` |
| `POST` | `/instances/{name}/stop` | Stop instance | 200 | 404, 409 `NOT_RUNNING` |
| `POST` | `/instances/{name}/restart` | Restart instance | 200 | 404 |
| `GET` | `/instances/{name}/logs` | Get instance logs | 200 | 404 |

Error responses include a machine-readable `code` field:
```json
{"error": "Instance not found", "code": "INSTANCE_NOT_FOUND", "timestamp": "..."}
```

### Create Instance

```bash
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "my-model",
    "model_id": "BAAI/bge-small-en-v1.5",
    "gpu_id": 0,
    "max_batch_tokens": 16384,
    "max_concurrent_requests": 512
  }'
```

**Required Fields:**
- `name` - Unique instance name
- `model_id` - HuggingFace model ID

**Optional Fields:**
- `port` - HTTP port (auto-assigned if omitted)
- `gpu_id` - GPU to pin instance to (omit to use all GPUs)
- `max_batch_tokens` - Max tokens per batch (default: 16384)
- `max_concurrent_requests` - Max concurrent requests (default: 512)
- `pooling` - Pooling method (e.g., "splade" for sparse models)

---

## Configuration

### Environment Variables

```bash
TEI_MANAGER_API_PORT=9000           # REST API port
TEI_MANAGER_GRPC_PORT=9001          # gRPC multiplexer port
TEI_MANAGER_STATE_FILE=/data/state.toml
TEI_BINARY_PATH=/usr/local/bin/text-embeddings-router
```

### Config File

```toml
api_port = 9000
grpc_port = 9001
state_file = "/data/state.toml"
health_check_interval_secs = 30
max_instances = 10

# Seed instances
[[instances]]
name = "bge-small"
model_id = "BAAI/bge-small-en-v1.5"
gpu_id = 0
```

---

## Examples

### Multi-GPU Setup

```bash
# GPU 0: Small model for low-latency
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{"name": "fast", "model_id": "BAAI/bge-small-en-v1.5", "gpu_id": 0}'

# GPU 1: Large model for quality
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{"name": "quality", "model_id": "BAAI/bge-large-en-v1.5", "gpu_id": 1}'

# Route requests to either via gRPC
grpcurl -plaintext -d '{"target": {"instance_name": "fast"}, "request": {"inputs": "Quick query"}}' \
  localhost:9001 tei_multiplexer.v1.TeiMultiplexer/Embed

grpcurl -plaintext -d '{"target": {"instance_name": "quality"}, "request": {"inputs": "Important document"}}' \
  localhost:9001 tei_multiplexer.v1.TeiMultiplexer/Embed
```

### Sparse Embeddings (SPLADE)

```bash
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "splade",
    "model_id": "naver/splade-cocondenser-ensembledistil",
    "pooling": "splade"
  }'

# Generate sparse embeddings
grpcurl -plaintext -d '{
  "target": {"instance_name": "splade"},
  "request": {"inputs": "Information retrieval"}
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/EmbedSparse
```

---

## Development

```bash
# Install just: cargo install just
just --list              # Show all available commands

# Common workflows
just test                # Run unit tests
just check               # Format check + clippy + all tests
just coverage            # Generate HTML coverage report
just docker-build        # Build Docker image
just pre-commit          # Run before committing
```

---

## Documentation

- **[DESIGN.md](DESIGN.md)** - Architecture and design decisions
- **[docs/GRPC_MULTIPLEXER.md](docs/GRPC_MULTIPLEXER.md)** - Full gRPC API reference
- **[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)** - Production deployment guide
- **[docs/MTLS.md](docs/MTLS.md)** - mTLS configuration

---

## Known Limitations

- **Single host only** - No clustering or multi-node coordination
- **No request queuing** - Requests exceeding TEI's `max_concurrent_requests` return errors immediately
- **No per-tenant auth** - mTLS authenticates connections, not individual requests
- **Port range required** - Each instance needs an HTTP port; plan your port range accordingly

### Future Directions

- Model-based routing (route by `model_id` instead of `instance_name`)
- HTTP embedding endpoint on manager (avoid direct TEI access)
- Metrics-based instance recommendations

---

## Versioning

TEI Manager follows [Semantic Versioning](https://semver.org/):

- **MAJOR** - Breaking changes to REST/gRPC APIs or config format
- **MINOR** - New features, backward-compatible
- **PATCH** - Bug fixes only

**Docker tag format:** `{manager_version}-tei-{tei_version}[-{arch}]`

The manager version tracks our API stability. The TEI version tracks the embedded TEI binary. We test against TEI's gRPC interface and will bump MINOR if TEI changes require manager updates.

**Current stability:**
- REST API: Stable since v0.4.0
- gRPC API: Stable since v0.3.0
- Config format: Stable since v0.1.0

---

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.
