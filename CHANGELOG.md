# Changelog

All notable changes to TEI Manager will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2025-11-26

### Changed
- Reduced allocations in hot paths for ~5% improvement on Arrow batch operations
- Arrow embed_arrow: build requests directly from Arrow array, pre-allocate flat embedding buffer
- Log handler: only allocate strings for requested slice
- Metrics: use static strings for metric names and label keys

## [0.5.0] - 2025-11-26

### Changed
- Updated Apache Arrow from 56 to 57 (IPC wire format remains compatible with v56 clients)
- Updated criterion from 0.5 to 0.7
- Benchmark code now uses `std::hint::black_box` instead of deprecated `criterion::black_box`

## [0.4.0] - 2025-11-26

### Added
- gRPC request timeouts (configurable via `grpc_request_timeout_secs`, default 30s)
- Graceful shutdown for gRPC server
- Connection pool pruning for idle/orphaned connections
- Security scanning in CI (cargo-audit, cargo-deny)
- Dependabot for automated dependency updates

### Changed
- **Breaking**: Error responses now include `code` field: `{"error": "...", "code": "INSTANCE_NOT_FOUND", "timestamp": "..."}`
- Unified error handling with structured error types and codes
- Some HTTP status codes refined (e.g., port allocation failures return 422)

## [0.3.0] - 2025-11-25

### Added
- **gRPC Multiplexer**: Unified gRPC endpoint for routing embedding requests to multiple TEI instances
  - Full TEI gRPC API support (Embed, EmbedSparse, EmbedAll, Rerank, Tokenize, Decode)
  - Streaming RPC support for batch processing
  - Arrow IPC batch embedding via `EmbedArrow` endpoint with LZ4 compression
  - Connection pooling with lazy connection creation
  - Instance-based routing via `target.instance_name`
- **Benchmark Client** (`bench-client`): Unified CLI tool for load testing
  - Standard mode: concurrent single-text requests
  - Arrow mode: batched Arrow IPC requests for high throughput
  - Configurable concurrency, batch size, and request counts
- **mTLS Authentication**: Pluggable authentication framework
  - `AuthProvider` trait for custom authentication providers
  - `MtlsProvider` for mutual TLS certificate validation
  - Subject and SAN verification options
- **Instance Readiness Checks**: gRPC-based health monitoring
  - Automatic status transition from Starting → Running
  - Configurable health check intervals and failure thresholds
  - Auto-restart on consecutive failures
- **Criterion Benchmarks**: Performance testing suite
  - `embedding_overhead`: Direct vs multiplexer latency comparison
  - `concurrent_requests`: Parallel load scaling tests
  - `streaming_requests`: Batch streaming performance
  - `arrow_batch`: Arrow IPC vs streaming comparison
- **Development Tooling**:
  - `just bench-start/stop/status`: Local benchmark environment management
  - `just bench-open`: Run benchmarks and open HTML report
- GPU architecture-specific Docker variants (Ada Lovelace, Hopper)

### Changed
- Docker images now include `bench-client` binary
- Health checks use gRPC Info RPC instead of HTTP
- Improved error messages for instance lifecycle operations
- Updated Docker build process with multi-variant support

### Fixed
- Docker build: Install protobuf-compiler in builder stage
- Docker build: Copy benches directory for Cargo.toml parsing
- Test isolation: Added `#[serial]` to environment variable tests
- Connection pool management in high-concurrency scenarios

## [0.2.0] - 2025-11-25
*Unreleased*

## [0.1.0] - 2025-11-15

### Added
- Initial release of TEI Manager
- REST API for TEI instance management (create, start, stop, restart, delete)
- Dynamic port allocation for TEI instances
- State persistence via TOML file
- Docker image with S6 overlay for process supervision
- Prometheus metrics endpoint (`/metrics`)
- Health check endpoint (`/health`)
- Configurable via TOML file or environment variables
- Support for both CPU and GPU inference
- Integration with HuggingFace Text Embeddings Inference

[Unreleased]: https://github.com/nazq/tei-manager/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/nazq/tei-manager/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/nazq/tei-manager/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/nazq/tei-manager/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/nazq/tei-manager/compare/v0.1.0...v0.3.0
[0.1.0]: https://github.com/nazq/tei-manager/releases/tag/v0.1.0
