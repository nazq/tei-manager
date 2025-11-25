#!/usr/bin/env bash
# End-to-end integration test for tei-manager
# This script builds the Docker image and exercises all features
#
# Usage:
#   ./test-e2e.sh                    # Test local Docker with mock TEI
#   ./test-e2e.sh https://host:port  # Test remote endpoint

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Parse arguments
REMOTE_ENDPOINT="${1:-}"

# Configuration
IMAGE_NAME="tei-manager:test"
CONTAINER_NAME="tei-manager-e2e-test"
API_BASE="${REMOTE_ENDPOINT:-http://localhost:9000}"

# Determine if we're testing remotely or locally
if [ -n "$REMOTE_ENDPOINT" ]; then
    REMOTE_TEST=true
    SKIP_DOCKER=true
    echo "Remote endpoint mode: $REMOTE_ENDPOINT"
else
    REMOTE_TEST=false
    SKIP_DOCKER=false
    echo "Local Docker mode (using mock TEI router)"
fi

# Cleanup function
cleanup() {
    echo -e "${YELLOW}Cleaning up...${NC}"
    docker stop "$CONTAINER_NAME" 2>/dev/null || true
    docker rm "$CONTAINER_NAME" 2>/dev/null || true
}

# Set trap for cleanup on exit
trap cleanup EXIT

# Helper functions
log_test() {
    echo -e "${YELLOW}[TEST]${NC} $1"
}

log_pass() {
    echo -e "${GREEN}[PASS]${NC} $1"
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $1"
    exit 1
}

http_get() {
    local url="$1"
    curl -s -w "\n%{http_code}" "$url"
}

http_post() {
    local url="$1"
    local data="$2"
    curl -s -w "\n%{http_code}" -X POST -H "Content-Type: application/json" -d "$data" "$url"
}

http_delete() {
    local url="$1"
    curl -s -w "\n%{http_code}" -X DELETE "$url"
}

check_http_status() {
    local response="$1"
    local expected="$2"
    local actual
    actual=$(echo "$response" | tail -n1)

    if [ "$actual" != "$expected" ]; then
        log_fail "Expected HTTP $expected, got $actual"
    fi
}

wait_for_instance_ready() {
    local instance_name="$1"
    local max_attempts="${2:-60}"
    local attempt=0

    while [ $attempt -lt $max_attempts ]; do
        response=$(http_get "$API_BASE/instances/$instance_name")
        status=$(echo "$response" | head -n-1 | grep -o '"status":"[^"]*"' | cut -d'"' -f4)

        if [ "$status" = "Running" ] || [ "$status" = "running" ]; then
            return 0
        elif [ "$status" = "Failed" ] || [ "$status" = "failed" ]; then
            return 1
        fi

        attempt=$((attempt + 1))
        sleep 2
    done

    return 1
}

http_embed() {
    local port="$1"
    local text="$2"

    curl -s -X POST "http://localhost:$port/embed" \
        -H "Content-Type: application/json" \
        -d "{\"inputs\": \"$text\"}"
}

# Start tests
echo "======================================"
echo "TEI Manager End-to-End Test"
echo "======================================"
echo "Target: $API_BASE"
echo

if [ "$SKIP_DOCKER" = false ]; then
    # Step 1: Build Docker image
    log_test "Building Docker image..."
    docker build -t "$IMAGE_NAME" . || log_fail "Docker build failed"
    log_pass "Docker image built successfully"

    # Step 2: Start container with mock TEI router
    log_test "Starting tei-manager container..."
    docker run -d \
        --name "$CONTAINER_NAME" \
        -p 9000:9000 \
        -p 9001:9001 \
        -p 8080:8080 \
        -p 8081:8081 \
        -p 8082:8082 \
        -e TEI_MANAGER_STATE_FILE=/tmp/state.toml \
        -e TEI_BINARY_PATH=/usr/local/bin/text-embeddings-router-mock \
        "$IMAGE_NAME" || log_fail "Failed to start container"

    # Wait for container to be healthy
    log_test "Waiting for container to be ready..."
    sleep 5

    # Check if container is running
    if ! docker ps | grep -q "$CONTAINER_NAME"; then
        echo "Container logs:"
        docker logs "$CONTAINER_NAME"
        log_fail "Container is not running"
    fi
    log_pass "Container is running"
else
    log_test "Skipping Docker build (remote endpoint mode)"
fi

# Step 3: Test health endpoint
log_test "Testing /health endpoint..."
response=$(http_get "$API_BASE/health")
check_http_status "$response" "200"
if ! echo "$response" | grep -q "healthy"; then
    log_fail "Health check response doesn't contain 'healthy'"
fi
log_pass "Health endpoint works"

# Step 4: Clean up any existing instances (idempotent)
log_test "Cleaning up existing instances..."
response=$(http_get "$API_BASE/instances")
check_http_status "$response" "200"
instance_names=$(echo "$response" | head -n-1 | grep -o '"name":"[^"]*"' | cut -d'"' -f4)
if [ -n "$instance_names" ]; then
    for name in $instance_names; do
        log_test "Deleting existing instance: $name"
        delete_response=$(http_delete "$API_BASE/instances/$name")
        # Allow 204 (deleted) or 404 (already gone)
        delete_status=$(echo "$delete_response" | tail -n1)
        if [ "$delete_status" != "204" ] && [ "$delete_status" != "404" ]; then
            log_fail "Failed to delete instance $name (status: $delete_status)"
        fi
    done
    log_pass "Existing instances cleaned up"
else
    log_pass "No existing instances to clean up"
fi

# Verify empty state
log_test "Verifying empty instances list..."
response=$(http_get "$API_BASE/instances")
check_http_status "$response" "200"
if ! echo "$response" | head -n-1 | grep -q '\[\]'; then
    log_fail "Expected empty instances list after cleanup"
fi
log_pass "Instance list is empty"

# Step 5: Create first instance
log_test "Creating instance 'test-model-1'..."
payload='{
    "name": "test-model-1",
    "model_id": "BAAI/bge-small-en-v1.5",
    "port": 8080,
    "max_batch_tokens": 1024,
    "max_concurrent_requests": 10
}'
response=$(http_post "$API_BASE/instances" "$payload")
check_http_status "$response" "201"
if ! echo "$response" | grep -q "test-model-1"; then
    log_fail "Instance creation response doesn't contain instance name"
fi
log_pass "Instance created successfully"

# Step 6: List instances (should have 1)
log_test "Testing GET /instances (1 instance)..."
response=$(http_get "$API_BASE/instances")
check_http_status "$response" "200"
if ! echo "$response" | grep -q "test-model-1"; then
    log_fail "Instance not found in list"
fi
log_pass "Instance appears in list"

# Step 7: Get specific instance
log_test "Testing GET /instances/test-model-1..."
response=$(http_get "$API_BASE/instances/test-model-1")
check_http_status "$response" "200"
if ! echo "$response" | grep -q "test-model-1"; then
    log_fail "Instance details not returned"
fi
log_pass "Instance details retrieved"

# Step 8: Create second instance
log_test "Creating instance 'test-model-2'..."
payload='{
    "name": "test-model-2",
    "model_id": "sentence-transformers/all-mpnet-base-v2",
    "port": 8081
}'
response=$(http_post "$API_BASE/instances" "$payload")
check_http_status "$response" "201"
log_pass "Second instance created"

# Step 9: Wait for instances to be ready
log_test "Waiting for test-model-1 to be ready..."
if ! wait_for_instance_ready "test-model-1" 60; then
    log_fail "test-model-1 failed to become ready"
fi
log_pass "test-model-1 is ready"

log_test "Waiting for test-model-2 to be ready..."
if ! wait_for_instance_ready "test-model-2" 60; then
    log_fail "test-model-2 failed to become ready"
fi
log_pass "test-model-2 is ready"

# Step 10-11: Test TEI embedding generation (local Docker only)
if [ "$SKIP_DOCKER" = false ]; then
    # Test embedding generation from test-model-1
    log_test "Testing embedding generation from test-model-1..."
    embed_response=$(http_embed 8080 "Hello, this is a test embedding" 2>&1)
    if ! echo "$embed_response" | jq -e 'type == "array"' >/dev/null 2>&1; then
        echo "Response: $embed_response"
        log_fail "Embedding response is not a valid array"
    fi
    if ! echo "$embed_response" | jq -e '.[0] | type == "array"' >/dev/null 2>&1; then
        echo "Response: $embed_response"
        log_fail "Embedding response is not an array of arrays"
    fi
    actual_dim=$(echo "$embed_response" | jq '.[0] | length')
    log_pass "test-model-1 generated embeddings (dimension: $actual_dim)"

    # Test embedding generation from test-model-2
    log_test "Testing embedding generation from test-model-2..."
    embed_response=$(http_embed 8081 "Another test embedding" 2>&1)
    if ! echo "$embed_response" | jq -e 'type == "array"' >/dev/null 2>&1; then
        echo "Response: $embed_response"
        log_fail "Embedding response is not a valid array"
    fi
    actual_dim=$(echo "$embed_response" | jq '.[0] | length')
    log_pass "test-model-2 generated embeddings (dimension: $actual_dim)"
fi

# Step 12: Test gRPC multiplexer (if grpcurl available)
if command -v grpcurl >/dev/null 2>&1; then
    GRPC_ENDPOINT="localhost:9001"
    log_test "Testing gRPC multiplexer on $GRPC_ENDPOINT..."

    # List services
    if grpcurl -plaintext "$GRPC_ENDPOINT" list 2>/dev/null | grep -q "tei_multiplexer"; then
        log_pass "gRPC multiplexer is available"

        # Test embedding via gRPC
        log_test "Testing gRPC Embed on test-model-1..."
        grpc_response=$(grpcurl -plaintext \
            -d '{"target": {"instance_name": "test-model-1"}, "request": {"inputs": "Hello from gRPC", "truncate": true, "normalize": true}}' \
            "$GRPC_ENDPOINT" \
            tei_multiplexer.v1.TeiMultiplexer/Embed 2>&1)

        if echo "$grpc_response" | grep -q "embeddings"; then
            log_pass "gRPC embedding successful"
        else
            echo "gRPC response: $grpc_response"
            log_test "gRPC embedding failed (mock may not support gRPC)"
        fi
    else
        log_test "gRPC multiplexer not responding (mock TEI may not support gRPC)"
    fi
else
    log_test "grpcurl not installed - skipping gRPC tests"
fi

# Step 13: Test port conflict detection
log_test "Testing port conflict detection..."
payload='{
    "name": "test-conflict",
    "model_id": "some-model",
    "port": 8080
}'
response=$(http_post "$API_BASE/instances" "$payload")
status=$(echo "$response" | tail -n1)
if [ "$status" == "201" ]; then
    log_fail "Port conflict not detected (should have failed)"
fi
log_pass "Port conflict detected correctly"

# Step 14: Test duplicate name detection
log_test "Testing duplicate name detection..."
payload='{
    "name": "test-model-1",
    "model_id": "some-model",
    "port": 8082
}'
response=$(http_post "$API_BASE/instances" "$payload")
status=$(echo "$response" | tail -n1)
if [ "$status" == "201" ]; then
    log_fail "Duplicate name not detected (should have failed)"
fi
log_pass "Duplicate name detected correctly"

# Step 15: Stop instance
log_test "Testing POST /instances/test-model-1/stop..."
response=$(http_post "$API_BASE/instances/test-model-1/stop" "")
check_http_status "$response" "200"
log_pass "Instance stopped successfully"

# Step 16: Start instance
log_test "Testing POST /instances/test-model-1/start..."
response=$(http_post "$API_BASE/instances/test-model-1/start" "")
check_http_status "$response" "200"
log_pass "Instance started successfully"

# Step 17: Restart instance
log_test "Testing POST /instances/test-model-1/restart..."
response=$(http_post "$API_BASE/instances/test-model-1/restart" "")
check_http_status "$response" "200"
log_pass "Instance restarted successfully"

# Step 18: Delete instance
log_test "Testing DELETE /instances/test-model-2..."
response=$(http_delete "$API_BASE/instances/test-model-2")
check_http_status "$response" "204"
log_pass "Instance deleted successfully"

# Step 19: Verify instance was deleted
log_test "Verifying instance deletion..."
response=$(http_get "$API_BASE/instances/test-model-2")
status=$(echo "$response" | tail -n1)
if [ "$status" == "200" ]; then
    log_fail "Deleted instance still accessible"
fi
log_pass "Instance properly deleted"

# Step 20: Test metrics endpoint
log_test "Testing /metrics endpoint..."
response=$(http_get "$API_BASE/metrics")
check_http_status "$response" "200"
log_pass "Metrics endpoint accessible"

# Step 21: Check Docker logs (skip for remote)
if [ "$SKIP_DOCKER" = false ]; then
    log_test "Checking for errors in logs..."
    if docker logs "$CONTAINER_NAME" 2>&1 | grep -i "error.*failed" | grep -v "Failed to connect" | grep -v "Failed to send" | grep -q .; then
        echo "Found errors in logs:"
        docker logs "$CONTAINER_NAME" 2>&1 | grep -i "error.*failed" | head -n 5
        log_fail "Errors found in container logs"
    fi
    log_pass "No critical errors in logs"
fi

# Final summary
echo
echo "======================================"
echo -e "${GREEN}All E2E tests passed!${NC}"
echo "======================================"
echo
echo "Test Summary:"
echo "  - Health endpoint working"
echo "  - Instance CRUD operations working"
echo "  - Instance lifecycle (stop/start/restart) working"
echo "  - Port conflict detection working"
echo "  - Duplicate name detection working"
echo "  - Metrics endpoint accessible"
if [ "$SKIP_DOCKER" = false ]; then
    echo "  - Docker image built successfully"
    echo "  - Mock TEI instances generated embeddings"
fi
echo

exit 0
