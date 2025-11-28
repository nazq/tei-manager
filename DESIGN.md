# TEI Manager Design

This document describes the internal architecture and design decisions of TEI Manager.

## Architecture Overview

### System Components

```
┌───────────────────────────────────────────────────┐
│                   TEI Manager                     │
├───────────────────────────────────────────────────┤
│                                                   │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐         │
│  │   API    │  │ Registry │  │  State   │         │
│  │  Server  │──│ (HashMap)│──│ Manager  │         │
│  └──────────┘  └──────────┘  └──────────┘         │
│       │             │              │              │
│       │        ┌────┴────┐         │              │
│       │        │ Health  │         │              │
│       │        │ Monitor │         │              │
│       │        └─────────┘         │              │
│       │                            │              │
│  ┌────▼─────────────────────────┐  │              │
│  │   Instance Management        │  │              │
│  │  ┌──────┐  ┌──────┐  ┌─────┐ │  │              │
│  │  │ Inst │  │ Inst │  │ ... │ │  │              │
│  │  │  #1  │  │  #2  │  │     │ │  │              │
│  │  └──┬───┘  └──┬───┘  └─────┘ │  │              │
│  └─────┼─────────┼──────────────┘  │              │
└────────┼─────────┼─────────────────┼──────────────┘
         │         │                 │
    ┌────▼────┐ ┌──▼──────┐     ┌────▼────┐
    │   TEI   │ │   TEI   │     │  State  │
    │Instance │ │Instance │     │  File   │
    │  :8080  │ │  :8081  │     │  .toml  │
    └─────────┘ └─────────┘     └─────────┘
```

### Component Responsibilities

#### API Server (`src/api/`)
- HTTP REST API using Axum framework
- Request routing and validation
- JSON serialization/deserialization
- Error response formatting
- OpenAPI-compatible endpoints
- Instance logs endpoint with Python-style slicing

#### Registry (`src/registry.rs`)
- Thread-safe instance storage using `Arc<RwLock<HashMap>>`
- Concurrent read/write access
- Instance lookup, addition, removal
- Port and name conflict detection
- Max instance limit enforcement

#### State Manager (`src/state.rs`)
- Persistent state storage in TOML format
- Atomic file writes (write to temp, then rename)
- State restoration on startup
- Crash recovery with auto-restore

#### Health Monitor (`src/health.rs`)
- Background health checking per instance
- Configurable check intervals and initial delays
- Failure tracking and threshold-based restart
- Event-driven architecture with channels
- Graceful shutdown on termination

#### Authentication (`src/auth/`)
- Pluggable authentication provider architecture
- mTLS (mutual TLS) certificate-based authentication
- Support for both native TLS and reverse proxy TLS
- Certificate CN/SAN validation
- Middleware-based request authentication
- Designed for future auth providers (OAuth, JWT, etc.)

#### Instance (`src/instance.rs`)
- TEI process lifecycle management
- Spawn, stop, restart operations
- PID tracking (runtime only, not persisted)
- GPU assignment via CUDA_VISIBLE_DEVICES
- Log file redirection with configurable directory
- Graceful shutdown with SIGTERM → SIGKILL fallback
- Process cleanup on Drop

#### Metrics (`src/metrics.rs`)
- Prometheus metrics export
- Instance creation/deletion counters
- Health check failure tracking
- Instance restart counters
- Active instance gauge

#### gRPC Multiplexer (`src/grpc/`)
- Unified gRPC endpoint for routing requests to TEI instances
- Connection pooling with lazy connection creation and idle pruning
- Support for all TEI RPC methods (embed, rerank, tokenize, etc.)
- Bidirectional streaming support
- Configurable request timeouts (default 30s)
- Graceful shutdown support
- Minimal overhead: 1-22% depending on workload

## Key Design Decisions

### Thread-Safe Concurrency

**Decision:** Use `Arc<RwLock<HashMap>>` for instance registry

**Rationale:**
- Multiple threads need read access (API handlers, health checks)
- Infrequent writes (instance creation/deletion)
- RwLock allows concurrent readers
- Arc provides shared ownership

**Trade-offs:**
- RwLock can block on write contention
- Considered DashMap but HashMap + RwLock is simpler for this use case
- gRPC multiplexer uses DashMap for its connection pool (higher concurrency)

### Atomic State Persistence

**Decision:** Write state to temporary file, then atomic rename

**Implementation:**
```rust
let temp_path = format!("{}.tmp", path);
fs::write(&temp_path, toml_str)?;
fs::rename(temp_path, path)?;  // POSIX atomic operation
```

**Rationale:**
- Prevents corruption on crash during write
- POSIX guarantees atomicity of rename
- Old state remains intact if write fails

**Alternatives Considered:**
- Direct write: Risks corruption
- Write-ahead logging: Over-engineered for this use case

### No PID Persistence

**Decision:** PIDs are tracked in memory only, not saved to state file

**Rationale:**
- PIDs are invalid across restarts
- State file represents desired configuration, not runtime state
- On restore, instances start with new PIDs

**Implications:**
- Restart always creates new processes
- Cannot resume stopped instances after manager restart
- Simple and predictable behavior

### Immutable Instance Configuration

**Decision:** No PATCH/update endpoint; use DELETE + CREATE to modify

**Rationale:**
- TEI processes cannot change model after startup
- Config changes require process restart anyway
- Simpler API and implementation
- Clearer semantics

**Alternative:** PATCH endpoint that deletes and recreates internally
- Rejected due to atomic operation concerns
- User should explicitly delete and recreate

### Health Check Delays

**Decision:** Configurable initial delay before first health check

**Rationale:**
- Model loading takes time (especially large models)
- Avoid false positives during startup
- Default 60s delay allows most models to load

**Configuration:**
```toml
health_check_initial_delay_secs = 60
health_check_interval_secs = 30
max_failures_before_restart = 3
```

### Process Ownership

**Decision:** Process handles own child lifetime via Drop trait

**Implementation:**
```rust
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        self.stop().ok();  // Graceful shutdown on drop
    }
}
```

**Rationale:**
- Automatic cleanup on manager shutdown
- Prevents orphaned processes
- RAII pattern ensures resource cleanup

**Graceful Shutdown:**
1. Send SIGTERM to child
2. Wait up to 5 seconds
3. Send SIGKILL if still running

### Auto-Recovery Strategy

**Decision:** Configurable auto-restart on health check failures

**Rationale:**
- Models can crash or hang
- Automatic recovery improves availability
- Threshold prevents restart loops

**Implementation:**
```rust
if failures >= max_failures {
    restart_instance().await?;
    reset_failure_count();
}
```

**Configuration:**
```toml
max_failures_before_restart = 3  # 0 to disable
```

### gRPC Multiplexer Design

**Decision:** Single unified gRPC endpoint with routing

**Rationale:**
- Simplifies client configuration
- Enables load balancing and routing strategies
- Centralizes connection management

**Connection Pool:**
- Lazy connection creation (on-demand)
- DashMap for lock-free concurrent access
- Connection reuse via HTTP/2 multiplexing

**Routing:**
- Target specified in request protobuf field
- Supports instance name routing
- Future: Round-robin, model-based routing

**Performance:**
- Unary RPCs: 7-22% overhead
- Concurrent requests: 8-10% overhead (excellent scaling)
- Streaming RPCs: 1% overhead at batch size 20 (outstanding)

### Pluggable Authentication Architecture

**Decision:** Provider-based authentication system with middleware

**Rationale:**
- Support multiple authentication methods (mTLS, OAuth, JWT, custom)
- Enable/disable auth at runtime via configuration
- Centralize auth logic in middleware layer
- Support both native TLS and reverse proxy scenarios

**Implementation:**
```rust
pub trait AuthProvider: Send + Sync {
    async fn authenticate(&self, req: &Request) -> Result<AuthContext>;
}

pub struct AuthManager {
    providers: Vec<Arc<dyn AuthProvider>>,
}
```

**Current Providers:**
- mTLS: Certificate-based authentication with CN/SAN validation
- Future: OAuth2, JWT, API keys, LDAP

**Native TLS vs Proxy TLS:**
- Native TLS: Certificate in TLS connection, extracted by server
- Proxy TLS: Certificate passed via `X-SSL-Client-Cert` header
- Middleware supports both modes transparently

### Instance Log Management

**Decision:** Configurable log directory with fallback location

**Rationale:**
- Production needs persistent storage (`/data/logs`)
- Development/testing needs writable location (`/tmp`)
- Env var allows flexible deployment
- Fallback prevents failures in restricted environments

**Implementation:**
```bash
TEI_MANAGER_LOG_DIR=/data/logs  # Primary location
# Falls back to /tmp/tei-manager/logs if /data/logs not writable
```

**Log Endpoint:**
- Python-style slicing: `GET /instances/{name}/logs?start=0&end=10`
- Negative indices: `?start=-100` (last 100 lines)
- Half-open intervals: `[start, end)` matches Python semantics

## Project Structure

```
tei-manager/
├── src/
│   ├── main.rs              # Entry point, CLI, server setup
│   ├── lib.rs               # Public API exports
│   ├── config.rs            # Configuration loading & validation
│   ├── instance.rs          # TEI process management
│   ├── registry.rs          # Thread-safe instance registry
│   ├── state.rs             # State persistence (atomic writes)
│   ├── health.rs            # Health monitoring & auto-restart
│   ├── metrics.rs           # Prometheus metrics
│   ├── error.rs             # Error types & API errors
│   ├── api/
│   │   ├── mod.rs           # API module exports
│   │   ├── routes.rs        # Router setup & AppState
│   │   ├── handlers.rs      # Request handlers (inc. logs endpoint)
│   │   └── models.rs        # Request/Response DTOs
│   ├── auth/
│   │   ├── mod.rs           # Auth module exports & provider trait
│   │   ├── service.rs       # AuthManager & middleware
│   │   └── mtls.rs          # mTLS authentication provider
│   └── grpc/
│       ├── mod.rs           # gRPC module exports
│       ├── server.rs        # gRPC server setup
│       ├── multiplexer.rs   # Multiplexer service implementation
│       ├── pool.rs          # Connection pool management
│       └── proto/           # Generated protobuf code
├── proto/                   # Protocol buffer definitions
│   ├── tei/                 # TEI service protos
│   └── multiplexer/         # Multiplexer service protos
├── benches/                 # Criterion benchmarks
│   └── multiplexer_overhead.rs
├── tests/
│   ├── integration.rs       # Integration tests (in-process API tests)
│   ├── e2e/                  # E2E test helpers (testcontainers)
│   └── e2e_*.rs              # E2E tests using real TEI containers
├── config/
│   └── tei-manager.example.toml
├── docs/
│   ├── RUNPOD_DEPLOYMENT.md
│   └── refactor/            # Design docs and planning
├── Dockerfile               # Multi-stage Docker build
├── build.rs                 # Protobuf compilation
└── Cargo.toml
```

## Error Handling Strategy

### API Errors

All errors use the unified `TeiError` enum with structured error responses:

```json
{
  "error": "Instance 'my-instance' not found",
  "code": "INSTANCE_NOT_FOUND",
  "timestamp": "2025-01-15T10:30:00Z"
}
```

**Error Codes:**
- `INSTANCE_NOT_FOUND` - 404
- `INSTANCE_EXISTS`, `PORT_CONFLICT` - 409
- `INVALID_CONFIG`, `INVALID_PORT`, `INVALID_GPU_ID`, `VALIDATION_ERROR` - 400
- `MAX_INSTANCES_REACHED`, `PORT_ALLOCATION_FAILED` - 422
- `BACKEND_UNAVAILABLE` - 503
- `TIMEOUT` - 504
- `INTERNAL_ERROR`, `IO_ERROR` - 500

### Graceful Degradation

- Health check failures trigger restart, not shutdown
- State save failures logged but don't crash server
- Instance failures isolated (don't affect other instances)

## Testing Strategy

### Unit Tests
- Configuration validation
- Port/name conflict detection
- State serialization/deserialization
- Error handling paths

### Integration Tests
- Instance lifecycle operations
- API endpoint behavior
- Health monitoring
- State persistence

### End-to-End Tests
- Full Docker deployment
- Multi-instance scenarios
- Dense and sparse embeddings
- Lifecycle operations
- Metrics validation

### Benchmarks
- gRPC multiplexer overhead
- Concurrent request handling
- Streaming RPC performance

## Security Considerations

### Authentication & Authorization
- Optional mTLS certificate-based authentication
- X.509 certificate validation (CN/SAN matching)
- Supports both native TLS and reverse proxy deployments
- Pluggable provider architecture for custom auth methods
- Middleware-based request authentication

### Port Validation
- Reject privileged ports (< 1024)
- Detect port conflicts before starting instances

### Input Validation
- Instance names: No path separators
- Model IDs: Valid HuggingFace format
- Numeric ranges: Positive values only
- Certificate validation: Proper X.509 parsing

### Process Isolation
- Each TEI instance runs in separate process
- Crash isolation: One instance failure doesn't affect others
- Resource limits via TEI configuration
- Log file isolation per instance

### State File Security
- TOML format (human-readable)
- No credentials stored
- File permissions managed by OS
- TLS certificates stored separately (not in state)

## Performance Characteristics

### Memory Usage
- Base manager: ~10 MB
- Per instance: TEI memory footprint (model-dependent)
- Connection pool: Minimal (reuses HTTP/2 connections)

### CPU Usage
- Health checks: Minimal (HTTP ping every 30s)
- API server: Async I/O (Tokio runtime)
- No busy-waiting or polling

### Scalability
- Max instances: Configurable limit
- Concurrent API requests: Handled by Tokio thread pool
- gRPC multiplexer: Scales well to 20+ concurrent requests

### Benchmarks

All benchmarks use [Criterion](https://github.com/bheisler/criterion.rs) for statistical rigor.

**Running benchmarks:**
```bash
# Run all benchmarks
cargo bench

# Run specific benchmark suite
cargo bench --bench embedding
cargo bench --bench pool
cargo bench --bench registry

# Quick benchmark (fewer iterations)
cargo bench -- --quick

# Save baseline for comparison
cargo bench -- --save-baseline main
cargo bench -- --baseline main
```

#### Benchmark Results Summary

Results measured on AMD Ryzen 9 5900X, Ubuntu 22.04, Rust 1.91:

| Operation | Latency | Notes |
|-----------|---------|-------|
| **Pool Operations** | | |
| Pool get (hit) | ~12 ns | Lock-free DashMap lookup |
| Pool get + touch | ~25 ns | With timestamp update |
| Pool insert | ~430 ns | New connection entry |
| Pool remove | ~270 ns | |
| **Registry Operations** | | |
| Registry get | ~38-40 ns | RwLock read |
| Registry list (100 instances) | ~825 ns | Clone all instances |
| Registry add/remove | ~2.1 µs | Write lock + validation |
| **Port Allocation** | | |
| TCP bind check | ~1.6 µs | Single port availability check |
| Port allocation (empty range) | ~1.6 µs | First available port |
| Port allocation (90% full) | ~2.1 µs | Scan to find free port |
| **Arrow IPC** | | |
| Serialize 1K texts | ~7 µs | Input batch creation |
| Deserialize 1K texts | ~3.4 µs | Text extraction |
| Embedding result (1K items) | ~1.5 ms | 384-dim embeddings + LZ4 |
| Full roundtrip (1K items) | ~160 µs | Deserialize + process + serialize |

#### Scaling Characteristics

**Arrow batch processing:**
- Sub-linear scaling: 10K items takes ~1.9ms (vs 100 items at ~16µs)
- Throughput improves with batch size due to LZ4 compression efficiency

**Connection pool:**
- Constant time lookups regardless of pool size (DashMap)
- Concurrent reads scale linearly with reader count

**Registry:**
- Read operations O(1) for get, O(n) for list
- Write contention minimal with RwLock (readers don't block)

**Port allocation:**
- ~1.6µs per port check (TCP bind dominates)
- Well under 10ms even for 100-port range scans

#### Benchmark Methodology

1. **Isolation:** Each benchmark runs in its own process
2. **Warmup:** Criterion performs warmup iterations before measurement
3. **Statistical analysis:** Reports mean, std dev, and throughput
4. **Reproducibility:** Run `cargo bench` 3x to verify < 10% variance

See individual benchmark files for detailed test cases:
- `benches/embedding.rs` - Arrow IPC serialization/deserialization
- `benches/pool.rs` - DashMap connection pool operations
- `benches/registry.rs` - Registry contention and port allocation
- `benches/multiplexer_overhead.rs` - gRPC routing overhead (requires live TEI)

## Future Enhancements

### Planned Features
- Load balancing strategies (round-robin, least-loaded)
- Model-based routing (automatic instance selection)
- Instance pooling (pre-warmed instances)
- Metrics dashboards (Grafana integration)
- WebSocket API for real-time updates

### Considered but Deferred
- Dynamic GPU assignment (complex scheduling)
- Auto-scaling based on load (requires orchestration)
- Multi-node distributed deployment (out of scope)
- Instance migration (complex state management)
