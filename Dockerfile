# ============================================================================
# Builder stage - Compile tei-manager
# ============================================================================
FROM rust:1.91-slim-bookworm AS builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build release binary
RUN cargo build --release --locked

# ============================================================================
# TEI stage - Extract text-embeddings-router binary
# ============================================================================
FROM ghcr.io/huggingface/text-embeddings-inference:1.8.3-grpc AS tei

# ============================================================================
# Runtime stage - Debian slim base
# ============================================================================
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="TEI Manager"
LABEL org.opencontainers.image.description="Dynamic TEI Instance Manager"
LABEL org.opencontainers.image.source="https://github.com/nazq/tei-manager"
LABEL org.opencontainers.image.licenses="Apache-2.0"

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install uv for running Python scripts
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /bin/

# Pre-install Python so uv doesn't download it every time
RUN uv python install 3.14

# Copy tei-manager binary from builder
COPY --from=builder /build/target/release/tei-manager /usr/local/bin/tei-manager

# Copy real text-embeddings-router from official TEI image (default for production use)
COPY --from=tei /usr/local/bin/text-embeddings-router /usr/local/bin/text-embeddings-router

# Copy mock TEI router for testing only (use TEI_BINARY_PATH=/usr/local/bin/text-embeddings-router-mock)
COPY tests/mock-tei-router /usr/local/bin/text-embeddings-router-mock

# Make scripts executable
RUN chmod +x /usr/local/bin/text-embeddings-router /usr/local/bin/text-embeddings-router-mock

# Create data directory for state persistence
RUN mkdir -p /data && chmod 777 /data

# Copy example config
COPY config/tei-manager.example.toml /etc/tei-manager/config.example.toml

# Expose API port (default 9000)
EXPOSE 9000

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:9000/health || exit 1

# Override TEI entrypoint with our manager
ENTRYPOINT ["/usr/local/bin/tei-manager"]
CMD []
