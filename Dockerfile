# ============================================================================
# TEI Manager - Parameterized Public Image
# ============================================================================
#
# Build arguments for variant selection:
#   TEI_VARIANT       - TEI base image variant prefix (empty/"89-"/"hopper-"/"cpu-")
#   TEI_VERSION       - TEI version (default: 1.8.3)
#   VARIANT_SUFFIX    - Image tag suffix (empty/"ada"/"hopper"/"cpu")
#   VARIANT_NAME      - Human-readable variant name for labels
#   VARIANT_DESC      - Additional description for labels
#
# Usage:
#   # Standard GPU (multi-arch, default)
#   docker build -t tei-manager:latest .
#
#   # CPU (for CI/testing, no GPU required)
#   docker build \
#     --build-arg TEI_VARIANT=cpu- \
#     --build-arg VARIANT_SUFFIX=cpu \
#     --build-arg VARIANT_NAME="CPU" \
#     --build-arg VARIANT_DESC=" - CPU-only, no GPU required" \
#     -t tei-manager:latest-cpu .
#
#   # Ada Lovelace (RTX 4090/4080)
#   docker build \
#     --build-arg TEI_VARIANT=89- \
#     --build-arg VARIANT_SUFFIX=ada \
#     --build-arg VARIANT_NAME="Ada Lovelace" \
#     --build-arg VARIANT_DESC=" - Optimized for RTX 4090/4080" \
#     -t tei-manager:latest-ada .
#
#   # Hopper (H100/H200)
#   docker build \
#     --build-arg TEI_VARIANT=hopper- \
#     --build-arg VARIANT_SUFFIX=hopper \
#     --build-arg VARIANT_NAME="Hopper" \
#     --build-arg VARIANT_DESC=" for H100/H200 GPUs" \
#     -t tei-manager:latest-hopper .
#
# ============================================================================

# Build arguments
ARG TEI_VARIANT=
ARG TEI_VERSION=1.8.3
ARG VARIANT_SUFFIX=
ARG VARIANT_NAME=
ARG VARIANT_DESC=

# ============================================================================
# Chef stage - Prepare recipe for caching dependencies
# ============================================================================
FROM lukemathwalker/cargo-chef:latest-rust-1.91-slim-bookworm AS chef
WORKDIR /build

# ============================================================================
# Planner stage - Generate dependency recipe
# ============================================================================
FROM chef AS planner

# Copy manifests and source for dependency analysis
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto ./proto
COPY src ./src
COPY benches ./benches

# Generate recipe.json (list of dependencies)
RUN cargo chef prepare --recipe-path recipe.json

# ============================================================================
# Builder stage - Compile tei-manager with cached dependencies
# ============================================================================
FROM chef AS builder

# Install build dependencies (including musl-tools for static linking)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    musl-tools \
    && rm -rf /var/lib/apt/lists/*

# Add musl target for static linking (works on any Linux distro)
RUN rustup target add x86_64-unknown-linux-musl

# Copy recipe from planner stage
COPY --from=planner /build/recipe.json recipe.json

# Build dependencies only - this layer is cached unless Cargo.toml/Cargo.lock change
# This is the key optimization: dependencies are built in a separate layer
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json

# Copy build script and proto files for gRPC compilation
COPY build.rs ./
COPY proto ./proto

# Copy source code
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Copy benches for Cargo.toml references (not built, just needed for manifest parsing)
COPY benches ./benches

# Build the actual binaries - only recompiles if source changed
RUN cargo build --release --target x86_64-unknown-linux-musl --locked && \
    cargo build --release --target x86_64-unknown-linux-musl --bin bench-client --locked && \
    cp target/x86_64-unknown-linux-musl/release/tei-manager /tmp/tei-manager && \
    cp target/x86_64-unknown-linux-musl/release/bench-client /tmp/bench-client

# ============================================================================
# TEI stage - Extract text-embeddings-router binary
# ============================================================================
ARG TEI_VARIANT
ARG TEI_VERSION
FROM ghcr.io/huggingface/text-embeddings-inference:${TEI_VARIANT}${TEI_VERSION}-grpc AS tei

# ============================================================================
# Runtime stage - Use TEI image as base (has CUDA support)
# ============================================================================
ARG TEI_VARIANT
ARG TEI_VERSION
FROM ghcr.io/huggingface/text-embeddings-inference:${TEI_VARIANT}${TEI_VERSION}-grpc

ARG VARIANT_NAME
ARG VARIANT_DESC

LABEL org.opencontainers.image.title="TEI Manager${VARIANT_NAME:+ (${VARIANT_NAME})}"
LABEL org.opencontainers.image.description="Dynamic TEI Instance Manager${VARIANT_DESC}"
LABEL org.opencontainers.image.source="https://github.com/nazq/tei-manager"
LABEL org.opencontainers.image.licenses="Apache-2.0"

# S6 overlay version
ARG S6_OVERLAY_VERSION=3.2.0.2
ARG TARGETARCH

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    curl \
    ca-certificates \
    xz-utils \
    jq \
    && rm -rf /var/lib/apt/lists/*

# Map Docker TARGETARCH to s6-overlay arch naming
# Docker: amd64, arm64, arm/v7 -> s6-overlay: x86_64, aarch64, armhf
RUN case "${TARGETARCH}" in \
    "amd64")  S6_ARCH=x86_64  ;; \
    "arm64")  S6_ARCH=aarch64 ;; \
    "arm")    S6_ARCH=armhf   ;; \
    *)        S6_ARCH=${TARGETARCH} ;; \
    esac \
    && echo "S6_ARCH=${S6_ARCH}" > /tmp/s6_arch

# Install s6-overlay for proper init system
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz /tmp
RUN tar -C / -Jxpf /tmp/s6-overlay-noarch.tar.xz && rm /tmp/s6-overlay-noarch.tar.xz

RUN . /tmp/s6_arch && \
    curl -L "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-${S6_ARCH}.tar.xz" -o /tmp/s6-overlay-arch.tar.xz && \
    tar -C / -Jxpf /tmp/s6-overlay-arch.tar.xz && \
    rm /tmp/s6-overlay-arch.tar.xz /tmp/s6_arch

# Install uv for running Python scripts (downloads Python on-demand when needed)
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /bin/

# Copy static musl binaries from builder (works on any Linux distro)
COPY --from=builder /tmp/tei-manager /usr/local/bin/tei-manager
COPY --from=builder /tmp/bench-client /usr/local/bin/bench-client

# Copy benchmark scripts
COPY scripts/raw-gpu-test.py /usr/local/bin/raw-gpu-test.py
COPY scripts/bench.sh /usr/local/bin/bench

# Copy real text-embeddings-router from official TEI image
COPY --from=tei /usr/local/bin/text-embeddings-router /usr/local/bin/text-embeddings-router

# Make scripts and binaries executable
RUN chmod +x /usr/local/bin/text-embeddings-router \
    /usr/local/bin/raw-gpu-test.py \
    /usr/local/bin/bench-client \
    /usr/local/bin/bench

# Create data directory for state persistence
RUN mkdir -p /data && chmod 777 /data

# Copy example config
COPY config/tei-manager.toml /etc/tei-manager/config/tei-manager.toml

# Create s6 service directory for tei-manager
RUN mkdir -p /etc/s6-overlay/s6-rc.d/tei-manager/dependencies.d /etc/s6-overlay/s6-rc.d/user/contents.d

RUN echo '#!/command/execlineb -P' > /etc/s6-overlay/s6-rc.d/tei-manager/run && \
    echo '/usr/local/bin/tei-manager -c /etc/tei-manager/config/tei-manager.toml' >> /etc/s6-overlay/s6-rc.d/tei-manager/run && \
    chmod +x /etc/s6-overlay/s6-rc.d/tei-manager/run

# Mark service as longrun type
RUN echo 'longrun' > /etc/s6-overlay/s6-rc.d/tei-manager/type

# Add dependency on base (ensures proper startup order)
RUN touch /etc/s6-overlay/s6-rc.d/tei-manager/dependencies.d/base

# Add service to user bundle
RUN touch /etc/s6-overlay/s6-rc.d/user/contents.d/tei-manager

# Expose ports (9000 for HTTP API, 9001 for gRPC - matches default config)
EXPOSE 9000 9001

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD ["sh", "-c", "curl -f http://localhost:9000/health || exit 1"]

# S6 overlay init as PID 1
ENTRYPOINT ["/init"]
