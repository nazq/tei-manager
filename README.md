# TEI Manager

[![Rust](https://img.shields.io/badge/rust-1.91+-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![codecov](https://codecov.io/gh/nazq/tei-manager/branch/main/graph/badge.svg)](https://codecov.io/gh/nazq/tei-manager)
[![Docker](https://img.shields.io/badge/docker-ready-brightgreen.svg)](Dockerfile)
[![TEI](https://img.shields.io/badge/TEI-1.8.3-purple.svg)](https://github.com/huggingface/text-embeddings-inference)

Dynamic multi-instance manager for [HuggingFace Text Embeddings Inference](https://github.com/huggingface/text-embeddings-inference) (TEI). Run multiple embedding models simultaneously with intelligent resource management, health monitoring, and automatic recovery.

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
| `EmbedArrow` | **High-throughput batch embedding via Arrow IPC** |
| `Rerank` | Rerank documents by relevance |
| `Tokenize` | Tokenize text |
| `Info` | Get model information |

### Arrow Batch Embeddings

The `EmbedArrow` endpoint enables high-throughput batch processing using Apache Arrow IPC format with LZ4 compression:

```bash
# Using grpcurl with base64-encoded Arrow IPC
grpcurl -plaintext -d '{
  "target": {"instance_name": "bge-small"},
  "arrow_ipc": "<base64-encoded-arrow-ipc>",
  "truncate": true,
  "normalize": true
}' localhost:9001 tei_multiplexer.v1.TeiMultiplexer/EmbedArrow
```

**Benefits:**
- Process thousands of texts in a single request
- LZ4 compression reduces network overhead
- Efficient memory layout for batch processing
- Returns embeddings as Arrow FixedSizeList for zero-copy access

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

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/health` | Health check |
| `GET` | `/metrics` | Prometheus metrics |
| `GET` | `/instances` | List all instances |
| `GET` | `/instances/{name}` | Get instance details |
| `POST` | `/instances` | Create new instance |
| `DELETE` | `/instances/{name}` | Delete instance |
| `POST` | `/instances/{name}/start` | Start instance |
| `POST` | `/instances/{name}/stop` | Stop instance |
| `POST` | `/instances/{name}/restart` | Restart instance |

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

---

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.
