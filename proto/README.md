# TEI-Manager Proto Definitions

This directory contains protobuf definitions for TEI-Manager's gRPC services.

## Architecture

TEI-Manager wraps HuggingFace's Text Embeddings Inference (TEI) gRPC API with a multiplexer layer that enables:

1. **Instance Routing**: Route requests to specific TEI instances by name, model ID, or index
2. **Multi-Instance Management**: Run multiple TEI instances with different models simultaneously
3. **High-Performance Batching**: Arrow-based batch embedding for maximum throughput
4. **Unified API**: Single gRPC endpoint for all backend instances

## Structure

```
proto/
├── tei/v1/tei.proto                        # Upstream TEI proto (vendored)
└── tei_multiplexer/v1/multiplexer.proto    # Multiplexer wrapper
```

### TEI Proto (Vendored)

**Version**: v1.8.3
**Source**: https://github.com/huggingface/text-embeddings-inference/blob/v1.8.3/proto/tei.proto
**Purpose**: Original TEI gRPC service definitions

We vendor the upstream TEI proto to:
- Ensure version compatibility with TEI binary
- Enable compilation without external dependencies
- Track which TEI version we're compatible with

### Multiplexer Proto (Our Wrapper)

**Purpose**: Wraps TEI proto with routing and batch capabilities

Key additions:
- `Target` message: Specifies which instance to route to (by name, model, or index)
- `EmbedArrow` RPC: High-performance Arrow batch embedding
- Wrapper request/response types that add routing to all TEI RPCs

## Updating TEI Version

When upgrading the TEI version in the Dockerfile, update the proto files:

### 1. Download New TEI Proto

```bash
# Replace VERSION with target TEI version (e.g., v1.9.0)
VERSION=v1.9.0
curl -s "https://raw.githubusercontent.com/huggingface/text-embeddings-inference/${VERSION}/proto/tei.proto" \
  -o proto/tei/v1/tei.proto
```

### 2. Update Version in README

Edit this file to reflect the new version and date.

### 3. Regenerate Rust Code

```bash
cargo build
```

The build script (`build.rs`) uses `tonic-build` to automatically generate Rust code from `.proto` files.

### 4. Test Compatibility

```bash
# Run unit tests
just test

# Run integration tests (requires TEI binary)
just test-integration
```

### 5. Update Multiplexer Proto (if needed)

If TEI added new RPCs or changed message formats, update `proto/tei_multiplexer/v1/multiplexer.proto` to wrap the new functionality.

## Why Wrap Instead of Extend?

We wrap TEI's proto rather than extending it directly because:

1. **Clean Separation**: Keeps upstream TEI proto unchanged for easy updates
2. **Routing Layer**: Our wrapper adds instance routing without modifying TEI semantics
3. **Backward Compatibility**: Clients can migrate gradually (direct TEI or via multiplexer)
4. **Additional Features**: We can add features (like Arrow batching) without touching TEI proto

## Generated Code

Rust code is generated in `src/grpc/proto/` during build:
- `tei::v1::*` - TEI service and types
- `multiplexer::v1::*` - Multiplexer service and types

See `build.rs` for generation configuration.
