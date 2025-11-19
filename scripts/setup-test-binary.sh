#!/bin/bash
# Extract the real TEI binary from official Docker image for testing

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
BINARY_PATH="$PROJECT_ROOT/tests/text-embeddings-router"

# Check if binary already exists
if [ -f "$BINARY_PATH" ]; then
    echo "✓ TEI binary already exists at $BINARY_PATH"
    exit 0
fi

echo "📦 Extracting TEI binary from official image..."

# Use CPU image for CI, gRPC image for local (GPU)
if [ "${CI:-false}" = "true" ]; then
    IMAGE="ghcr.io/huggingface/text-embeddings-inference:cpu-1.8.3"
    echo "   Using CPU image for CI"
else
    IMAGE="ghcr.io/huggingface/text-embeddings-inference:1.8.3-grpc"
    echo "   Using gRPC image for local development"
fi

# Pull the official TEI image
docker pull "$IMAGE"

# Create a temporary container and copy the binary
CONTAINER_ID=$(docker create "$IMAGE")
docker cp "$CONTAINER_ID:/usr/local/bin/text-embeddings-router" "$BINARY_PATH"
docker rm "$CONTAINER_ID" > /dev/null

# Make executable
chmod +x "$BINARY_PATH"

echo "✅ TEI binary extracted to $BINARY_PATH"
echo "   Size: $(du -h "$BINARY_PATH" | cut -f1)"
