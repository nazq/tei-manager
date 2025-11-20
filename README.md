# TEI Manager

[![Rust](https://img.shields.io/badge/rust-1.91+-orange.svg)](https://www.rust-lang.org/)
[![Edition](https://img.shields.io/badge/edition-2024-blue.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![codecov](https://codecov.io/gh/nazq/tei-manager/branch/main/graph/badge.svg)](https://codecov.io/gh/nazq/tei-manager)
[![Docker](https://img.shields.io/badge/docker-ready-brightgreen.svg)](Dockerfile)
[![Tests](https://img.shields.io/badge/tests-passing-brightgreen.svg)](test-e2e.sh)
[![TEI](https://img.shields.io/badge/TEI-1.8.3-purple.svg)](https://github.com/huggingface/text-embeddings-inference)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](CONTRIBUTING.md)

Dynamic multi-instance manager for [HuggingFace Text Embeddings Inference](https://github.com/huggingface/text-embeddings-inference) (TEI). Run multiple embedding models simultaneously with intelligent resource management, health monitoring, and automatic recovery.

---

## 🚀 Features

### Core Capabilities
- 🔄 **Dynamic Instance Management** - Create, start, stop, restart, and delete TEI instances via REST API
- 🎯 **Multi-GPU Support** - Pin instances to specific GPUs or share across all available GPUs
- 🌐 **gRPC Multiplexer** - Unified gRPC endpoint for routing requests to multiple instances (1-22% overhead)
- 💾 **State Persistence** - Automatic state saving with atomic writes and crash recovery
- 🏥 **Health Monitoring** - Continuous health checks with configurable auto-restart on failure
- 📊 **Prometheus Metrics** - Built-in metrics export for monitoring instance lifecycle and operations
- 🔒 **Resource Validation** - Port conflict detection, duplicate name prevention, and max instance limits
- ⚡ **Zero Downtime** - Graceful shutdown handling and instance lifecycle management

### Supported Models
- ✅ **Dense Embeddings** - BGE, E5, Sentence-Transformers, all-MiniLM, etc.
- ✅ **Sparse Embeddings** - SPLADE models with sparse vector output
- ✅ **Auto-Detection** - Fetches model metadata from HuggingFace for dimension validation

### API Features
- 🌐 RESTful JSON API with OpenAPI-compatible endpoints
- 📝 Instance CRUD operations with detailed status reporting
- 🔍 Individual instance inspection with uptime, restarts, and health stats
- 🎮 Lifecycle controls (start/stop/restart) with process management
- 📈 Prometheus metrics endpoint at `/metrics`

---

## 📋 Table of Contents

- [Quick Start](#-quick-start)
- [RunPod Deployment](#-runpod-deployment)
- [Installation](#-installation)
- [Configuration](#-configuration)
- [API Reference](#-api-reference)
- [gRPC Multiplexer](#-grpc-multiplexer)
- [Examples](#-examples)
- [Development](#-development)
- [Testing](#-testing)
- [Documentation](#-documentation)
- [Contributing](#-contributing)
- [License](#-license)

---

## 🏁 Quick Start

### Using Docker (Recommended)

```bash
# Build the image
docker build -t tei-manager:latest .

# Run with default config
docker run -d \
  --name tei-manager \
  -p 9000:9000 \
  -p 8080-8089:8080-8089 \
  -e TEI_MANAGER_STATE_FILE=/data/state.toml \
  -v $(pwd)/data:/data \
  tei-manager:latest

# Create your first instance
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "bge-small",
    "model_id": "BAAI/bge-small-en-v1.5",
    "port": 8080
  }'

# Generate embeddings
curl -X POST http://localhost:8080/embed \
  -H "Content-Type: application/json" \
  -d '{"inputs": "Hello world"}'
```

### Using Cargo

```bash
# Build from source
cargo build --release

# Run with custom config
./target/release/tei-manager --config config/tei-manager.example.toml

# Or with environment variables
TEI_MANAGER_API_PORT=9000 \
TEI_MANAGER_STATE_FILE=/tmp/state.toml \
./target/release/tei-manager --log-format pretty
```

---

## ☁️ RunPod Deployment

Deploy TEI Manager on RunPod's GPU cloud platform for scalable embedding inference.

### Quick Deploy

1. **Create Pod** with the following settings:
   - **Container Image**: `nazq/tei-manager:latest`
   - **Container Disk**: 20 GB
   - **Volume**: 50 GB (recommended for model caching)
   - **Expose HTTP Ports**: `8000,9000`
   - **Expose TCP Ports**: `8080-8089`

2. **Set Environment Variables**:
   ```bash
   TEI_MANAGER_API_PORT=8000
   TEI_MANAGER_STATE_FILE=/workspace/state.toml
   RUST_LOG=info
   ```

3. **Access Your Instance**:
   ```bash
   # Your pod will be available at:
   https://{POD_ID}-8000.proxy.runpod.net

   # Create a TEI instance
   curl -X POST https://{POD_ID}-8000.proxy.runpod.net/instances \
     -H "Content-Type: application/json" \
     -d '{
       "name": "bge-small",
       "model_id": "BAAI/bge-small-en-v1.5",
       "port": 8080,
       "gpu_id": 0
     }'

   # Generate embeddings
   curl -X POST https://{POD_ID}-8080.proxy.runpod.net/embed \
     -H "Content-Type: application/json" \
     -d '{"inputs": "Hello from RunPod!"}'
   ```

**📚 Full Guide**: See [docs/RUNPOD_DEPLOYMENT.md](docs/RUNPOD_DEPLOYMENT.md) for:
- Complete deployment templates
- Multi-GPU configuration
- Production best practices
- Troubleshooting guide

---

## 💿 Installation

### Prerequisites

- **Rust 1.91+** with Edition 2024 support
- **Docker** (optional, for containerized deployment)
- **Text Embeddings Inference binary** or use the provided mock for testing

### From Source

```bash
git clone https://github.com/nazq/tei-manager.git
cd tei-manager
cargo build --release
```

### Docker Image

The Docker image includes:
- ✅ Pre-compiled `tei-manager` binary
- ✅ Real `text-embeddings-router` from official TEI image (1.8.3-grpc, default)
- ✅ Mock TEI router for testing (`/usr/local/bin/text-embeddings-router-mock`)
- ✅ Python 3.14 via `uv` for mock router execution

```bash
docker build -t tei-manager:latest .

# Run with real TEI binary (default)
docker run -d -p 9000:9000 tei-manager:latest

# Run with mock for testing (use TEI_BINARY_PATH env var)
docker run -d -p 9000:9000 \
  -e TEI_BINARY_PATH=/usr/local/bin/text-embeddings-router-mock \
  tei-manager:latest
```

---

## ⚙️ Configuration

### Configuration File

Create a `tei-manager.toml` file (see [config/tei-manager.example.toml](config/tei-manager.example.toml)):

```toml
# API server port
api_port = 9000

# State file location for persistence
state_file = "/data/tei-manager-state.toml"

# Health check settings
health_check_interval_secs = 30
health_check_initial_delay_secs = 60
max_failures_before_restart = 3

# Graceful shutdown timeout
graceful_shutdown_timeout_secs = 30

# Auto-restore instances on restart
auto_restore_on_restart = true

# Maximum number of instances (optional)
max_instances = 5

# Seed instances on first boot
[[instances]]
name = "bge-small"
model_id = "BAAI/bge-small-en-v1.5"
port = 8080
max_batch_tokens = 16384
max_concurrent_requests = 512
# gpu_id = 0  # Optional: pin to GPU 0

[[instances]]
name = "all-mpnet"
model_id = "sentence-transformers/all-mpnet-base-v2"
port = 8081
gpu_id = 1  # Pin to GPU 1
```

### Environment Variables

Override config file values with environment variables:

```bash
TEI_MANAGER_API_PORT=9000
TEI_MANAGER_STATE_FILE=/data/state.toml
TEI_MANAGER_HEALTH_CHECK_INTERVAL=30
TEI_BINARY_PATH=/usr/local/bin/text-embeddings-router  # Path to TEI binary
```

**Docker Users:** The Docker image includes both real and mock TEI binaries:
- `/usr/local/bin/text-embeddings-router` - Real TEI binary (default, 838MB)
- `/usr/local/bin/text-embeddings-router-mock` - Mock for testing (5KB)

To use the mock for testing:
```bash
docker run -e TEI_BINARY_PATH=/usr/local/bin/text-embeddings-router-mock tei-manager:latest
```

### CLI Arguments

```bash
tei-manager \
  --config /path/to/config.toml \
  --port 9000 \
  --log-level info \
  --log-format json
```

**Options:**
- `--config <PATH>` - Path to configuration file
- `--port <PORT>` - Override API port
- `--log-level <LEVEL>` - Log level: trace, debug, info, warn, error (default: info)
- `--log-format <FORMAT>` - Log format: json, pretty (default: json)

---

## 🌐 API Reference

### Base URL
```
http://localhost:9000
```

### Endpoints

#### Health Check
```http
GET /health
```

**Response:**
```json
{
  "status": "healthy",
  "timestamp": "2025-11-18T20:00:00Z"
}
```

---

#### List Instances
```http
GET /instances
```

**Response:**
```json
[
  {
    "name": "bge-small",
    "model_id": "BAAI/bge-small-en-v1.5",
    "port": 8080,
    "status": "running",
    "pid": 12345,
    "created_at": "2025-11-18T19:00:00Z",
    "uptime_secs": 3600,
    "restarts": 0,
    "health_check_failures": 0,
    "last_health_check": "2025-11-18T19:59:30Z",
    "gpu_id": null,
    "prometheus_port": 9100
  }
]
```

---

#### Get Instance
```http
GET /instances/:name
```

**Response:** Same as individual instance object above.

---

#### Create Instance
```http
POST /instances
Content-Type: application/json
```

**Request Body:**
```json
{
  "name": "my-model",
  "model_id": "BAAI/bge-small-en-v1.5",
  "port": 8080,
  "max_batch_tokens": 16384,
  "max_concurrent_requests": 512,
  "pooling": "splade",
  "gpu_id": 0,
  "prometheus_port": 9100,
  "extra_args": ["--dtype", "float16"]
}
```

**Required Fields:**
- `name` - Unique instance name (no path separators)
- `model_id` - HuggingFace model ID
- `port` - Port number (>= 1024, must be unique)

**Optional Fields:**
- `max_batch_tokens` - Default: 16384
- `max_concurrent_requests` - Default: 512
- `pooling` - Pooling method (e.g., "splade" for sparse models)
- `gpu_id` - GPU ID to pin instance to (omit to use all GPUs)
- `prometheus_port` - Prometheus metrics port (auto-assigned from 9100+, 0 to disable)
- `extra_args` - Additional CLI arguments for `text-embeddings-router`

**Response:** `201 Created` with instance details

---

#### Delete Instance
```http
DELETE /instances/:name
```

**Response:** `204 No Content`

---

#### Start Instance
```http
POST /instances/:name/start
```

**Response:** `200 OK` with instance details

---

#### Stop Instance
```http
POST /instances/:name/stop
```

**Response:** `200 OK` with instance details

---

#### Restart Instance
```http
POST /instances/:name/restart
```

**Response:** `200 OK` with instance details

---

#### Prometheus Metrics
```http
GET /metrics
```

**Response:** Prometheus text format

**Metrics:**
- `tei_manager_instances_created_total` - Counter
- `tei_manager_instances_deleted_total` - Counter
- `tei_manager_instances_count` - Gauge
- `tei_manager_health_check_failures_total` - Counter
- `tei_manager_instance_restarts_total` - Counter

---

## 📚 Examples

### Dense Embeddings (BGE)

```bash
# Create instance
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "bge-small",
    "model_id": "BAAI/bge-small-en-v1.5",
    "port": 8080
  }'

# Generate embeddings
curl -X POST http://localhost:8080/embed \
  -H "Content-Type: application/json" \
  -d '{"inputs": "Hello world"}'
```

**Response:**
```json
[[0.123, -0.456, 0.789, ...]] // 384 dimensions
```

### Sparse Embeddings (SPLADE)

```bash
# Create SPLADE instance
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "splade",
    "model_id": "naver/splade-cocondenser-ensembledistil",
    "port": 8081,
    "pooling": "splade"
  }'

# Generate sparse embeddings
curl -X POST http://localhost:8081/embed \
  -H "Content-Type: application/json" \
  -d '{"inputs": "Information retrieval"}'
```

**Response:**
```json
[{"1234": 2.5, "5678": 1.8, "9012": 0.9, ...}] // Sparse format
```

### Multi-GPU Setup

```bash
# GPU 0: Small model
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "bge-small-gpu0",
    "model_id": "BAAI/bge-small-en-v1.5",
    "port": 8080,
    "gpu_id": 0
  }'

# GPU 1: Large model
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "bge-large-gpu1",
    "model_id": "BAAI/bge-large-en-v1.5",
    "port": 8081,
    "gpu_id": 1
  }'

# All GPUs: Shared instance
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "e5-base-shared",
    "model_id": "intfloat/e5-base-v2",
    "port": 8082
  }'
```

### Instance Lifecycle

```bash
# Stop instance
curl -X POST http://localhost:9000/instances/bge-small/stop

# Start instance
curl -X POST http://localhost:9000/instances/bge-small/start

# Restart instance (useful after model updates)
curl -X POST http://localhost:9000/instances/bge-small/restart

# Delete instance
curl -X DELETE http://localhost:9000/instances/bge-small
```

---

## 🛠️ Development

### Build

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test

# Run clippy
cargo clippy

# Format code
cargo fmt
```

---

## 📚 Documentation

- **[DESIGN.md](DESIGN.md)** - Internal architecture and design decisions
- **[docs/GRPC_MULTIPLEXER.md](docs/GRPC_MULTIPLEXER.md)** - gRPC multiplexer guide and benchmarks
- **[docs/RUNPOD_DEPLOYMENT.md](docs/RUNPOD_DEPLOYMENT.md)** - RunPod deployment guide
- **[CONTRIBUTING.md](CONTRIBUTING.md)** - Contribution guidelines

---

## 🧪 Testing

### Unit Tests

```bash
cargo test
```

**Coverage:**
- Configuration validation
- Port conflict detection
- Duplicate name detection
- Instance name validation

### End-to-End Tests

```bash
./test-e2e.sh
```

**Test Coverage:**
- ✅ Docker image build
- ✅ Container health checks
- ✅ Instance CRUD operations
- ✅ Dense embedding generation (bge-small: 384d, all-mpnet: 768d)
- ✅ Sparse embedding generation (SPLADE: sparse format)
- ✅ Embedding dimension validation against model metadata
- ✅ Port conflict detection
- ✅ Duplicate name detection
- ✅ Instance lifecycle (start/stop/restart)
- ✅ Prometheus metrics endpoint
- ✅ Log error checking

**Requirements:**
- Docker
- `curl`, `jq`, `grep` (standard Linux tools)

---

## 🌐 gRPC Multiplexer

Unified gRPC endpoint for routing embedding requests to multiple TEI instances.

**Features:**
- Single connection point for all instances
- Dynamic routing by instance name
- Full TEI API support (embed, rerank, tokenize, streaming, etc.)
- Low overhead: 1-22% depending on workload

**Quick Example:**
```python
import grpc
from tei_manager.grpc.proto.multiplexer.v1 import tei_multiplexer_pb2_grpc as mux_grpc

channel = grpc.insecure_channel('localhost:9001')
stub = mux_grpc.TeiMultiplexerStub(channel)

# Route to specific instance by name
response = stub.Embed(create_request(target="bge-small", text="Hello"))
```

**📚 Full Guide**: See [docs/GRPC_MULTIPLEXER.md](docs/GRPC_MULTIPLEXER.md) for:
- Complete API reference with Python and Rust examples
- Performance benchmarks (unary, concurrent, streaming)
- Routing strategies and best practices
- Troubleshooting guide

---

## 🤝 Contributing

We love contributions! See [CONTRIBUTING.md](CONTRIBUTING.md) for detailed guidelines.

**Quick Links:**
- 🐛 [Report a Bug](https://github.com/nazq/tei-manager/issues/new?labels=bug)
- ✨ [Request a Feature](https://github.com/nazq/tei-manager/issues/new?labels=enhancement)
- 📖 [Improve Documentation](https://github.com/nazq/tei-manager/issues/new?labels=documentation)

---

## 📄 License

This project is licensed under the **Apache License 2.0** - see the [LICENSE](LICENSE) file for details.

---

## 🙏 Acknowledgments

- [HuggingFace Text Embeddings Inference](https://github.com/huggingface/text-embeddings-inference) - The underlying TEI engine
- [Axum](https://github.com/tokio-rs/axum) - Ergonomic web framework
- [Tokio](https://tokio.rs/) - Async runtime for Rust

---

## 📞 Support

- 💬 [GitHub Discussions](https://github.com/nazq/tei-manager/discussions)
- 🐛 [Issue Tracker](https://github.com/nazq/tei-manager/issues)
- 📧 Email: [your-email@example.com](mailto:your-email@example.com)

---

<div align="center">

[![GitHub stars](https://img.shields.io/github/stars/nazq/tei-manager?style=social)](https://github.com/nazq/tei-manager)
[![GitHub forks](https://img.shields.io/github/forks/nazq/tei-manager?style=social)](https://github.com/nazq/tei-manager/fork)

</div>
