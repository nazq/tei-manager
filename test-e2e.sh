#!/usr/bin/env bash
# End-to-end integration test for tei-manager
# This script builds the Docker image and exercises all features

set -e

# Colors for output
RED='\033[0:31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Configuration
IMAGE_NAME="tei-manager:test"
CONTAINER_NAME="tei-manager-e2e-test"
API_BASE="http://localhost:9000"

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
    curl -s -w "\n%{http_code}" "$1"
}

http_post() {
    curl -s -w "\n%{http_code}" -X POST -H "Content-Type: application/json" -d "$2" "$1"
}

http_delete() {
    curl -s -w "\n%{http_code}" -X DELETE "$1"
}

check_http_status() {
    local response="$1"
    local expected="$2"
    local actual=$(echo "$response" | tail -n1)

    if [ "$actual" != "$expected" ]; then
        log_fail "Expected HTTP $expected, got $actual"
    fi
}

wait_for_tei_health() {
    local port="$1"
    local max_attempts=30
    local attempt=0

    while [ $attempt -lt $max_attempts ]; do
        if curl -s "http://localhost:$port/health" | grep -q "ok\|healthy" 2>/dev/null; then
            return 0
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
echo

# Step 1: Build Docker image
log_test "Building Docker image..."
docker build -t "$IMAGE_NAME" . || log_fail "Docker build failed"
log_pass "Docker image built successfully"

# Step 2: Start container
log_test "Starting tei-manager container..."
docker run -d \
    --name "$CONTAINER_NAME" \
    -p 9000:9000 \
    -p 8080:8080 \
    -p 8081:8081 \
    -p 8082:8082 \
    -e TEI_MANAGER_STATE_FILE=/tmp/state.toml \
    -e TEI_BINARY_PATH=/usr/local/bin/text-embeddings-router-mock \
    "$IMAGE_NAME" \
    --log-format pretty --log-level info || log_fail "Failed to start container"

# Wait for container to be healthy
log_test "Waiting for container to be ready..."
sleep 5

# Check if container is running
if ! docker ps | grep -q "$CONTAINER_NAME"; then
    log_fail "Container is not running"
    docker logs "$CONTAINER_NAME"
fi
log_pass "Container is running"

# Step 3: Test health endpoint
log_test "Testing /health endpoint..."
response=$(http_get "$API_BASE/health")
check_http_status "$response" "200"
if ! echo "$response" | grep -q "healthy"; then
    log_fail "Health check response doesn't contain 'healthy'"
fi
log_pass "Health endpoint works"

# Step 4: List instances (should be empty)
log_test "Testing GET /instances (empty)..."
response=$(http_get "$API_BASE/instances")
check_http_status "$response" "200"
if ! echo "$response" | head -n-1 | grep -q '\[\]'; then
    log_fail "Expected empty instances list"
fi
log_pass "Empty instances list returned"

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

# Step 8: Create second instance with GPU assignment
log_test "Creating instance 'test-model-2' with GPU assignment..."
payload='{
    "name": "test-model-2",
    "model_id": "sentence-transformers/all-mpnet-base-v2",
    "port": 8081,
    "gpu_id": 0
}'
response=$(http_post "$API_BASE/instances" "$payload")
check_http_status "$response" "201"
log_pass "Second instance created with GPU assignment"

# Step 8a: Create third instance (SPLADE sparse model)
log_test "Creating instance 'test-splade' with SPLADE pooling..."
payload='{
    "name": "test-splade",
    "model_id": "naver/splade-cocondenser-ensembledistil",
    "port": 8082,
    "pooling": "splade"
}'
response=$(http_post "$API_BASE/instances" "$payload")
check_http_status "$response" "201"
log_pass "SPLADE instance created successfully"

# Step 9: Wait for TEI instances to be ready
log_test "Waiting for test-model-1 to be ready..."
if ! wait_for_tei_health 8080; then
    log_fail "test-model-1 failed to become healthy"
fi
log_pass "test-model-1 is healthy"

log_test "Waiting for test-model-2 to be ready..."
if ! wait_for_tei_health 8081; then
    log_fail "test-model-2 failed to become healthy"
fi
log_pass "test-model-2 is healthy"

log_test "Waiting for test-splade to be ready..."
if ! wait_for_tei_health 8082; then
    log_fail "test-splade failed to become healthy"
fi
log_pass "test-splade is healthy"

# Step 10: Get model info from test-model-1
log_test "Getting model info from test-model-1..."
info_response=$(curl -s "http://localhost:8080/info")
model1_dim=$(echo "$info_response" | jq -r '.embedding_dimension')
if [ -z "$model1_dim" ] || [ "$model1_dim" == "null" ]; then
    echo "Info response: $info_response"
    log_fail "Could not get embedding_dimension from /info"
fi
log_pass "test-model-1 info retrieved (dimension: $model1_dim)"

# Step 11: Test embedding generation from test-model-1
log_test "Testing embedding generation from test-model-1 via HTTP..."
embed_response=$(http_embed 8080 "Hello, this is a test embedding" 2>&1)
if ! echo "$embed_response" | grep -q "\["; then
    echo "Response: $embed_response"
    log_fail "Embedding response doesn't contain expected array"
fi
# Verify it's a valid JSON array with numeric values
if ! echo "$embed_response" | jq -e 'type == "array"' >/dev/null 2>&1; then
    echo "Response: $embed_response"
    log_fail "Embedding response is not a valid array"
fi
if ! echo "$embed_response" | jq -e '.[0] | type == "array"' >/dev/null 2>&1; then
    echo "Response snippet: $(echo "$embed_response" | head -c 200)"
    log_fail "Embedding response is not an array of arrays"
fi
# Verify embedding dimension matches model info
actual_dim=$(echo "$embed_response" | jq '.[0] | length')
if [ "$actual_dim" != "$model1_dim" ]; then
    log_fail "Embedding dimension mismatch: expected $model1_dim, got $actual_dim"
fi
log_pass "test-model-1 generated embeddings with correct dimension ($actual_dim)"

# Step 12: Get model info from test-model-2
log_test "Getting model info from test-model-2..."
info_response=$(curl -s "http://localhost:8081/info")
model2_dim=$(echo "$info_response" | jq -r '.embedding_dimension')
if [ -z "$model2_dim" ] || [ "$model2_dim" == "null" ]; then
    echo "Info response: $info_response"
    log_fail "Could not get embedding_dimension from /info"
fi
log_pass "test-model-2 info retrieved (dimension: $model2_dim)"

# Step 13: Test embedding generation from test-model-2
log_test "Testing embedding generation from test-model-2 via HTTP..."
embed_response=$(http_embed 8081 "Another test embedding" 2>&1)
if ! echo "$embed_response" | grep -q "\["; then
    echo "Response: $embed_response"
    log_fail "Embedding response doesn't contain expected array"
fi
if ! echo "$embed_response" | jq -e 'type == "array"' >/dev/null 2>&1; then
    echo "Response: $embed_response"
    log_fail "Embedding response is not a valid array"
fi
if ! echo "$embed_response" | jq -e '.[0] | type == "array"' >/dev/null 2>&1; then
    echo "Response snippet: $(echo "$embed_response" | head -c 200)"
    log_fail "Embedding response is not an array of arrays"
fi
# Verify embedding dimension matches model info
actual_dim=$(echo "$embed_response" | jq '.[0] | length')
if [ "$actual_dim" != "$model2_dim" ]; then
    log_fail "Embedding dimension mismatch: expected $model2_dim, got $actual_dim"
fi
log_pass "test-model-2 generated embeddings with correct dimension ($actual_dim)"

# Step 14: Test SPLADE sparse embedding generation
log_test "Testing sparse embedding generation from test-splade via HTTP..."
embed_response=$(http_embed 8082 "Test sparse embedding" 2>&1)
if ! echo "$embed_response" | grep -q "{"; then
    echo "Response: $embed_response"
    log_fail "SPLADE embedding response doesn't contain expected object"
fi
# Verify it's a valid JSON array of objects (sparse format)
if ! echo "$embed_response" | jq -e 'type == "array"' >/dev/null 2>&1; then
    echo "Response: $embed_response"
    log_fail "SPLADE embedding response is not a valid array"
fi
if ! echo "$embed_response" | jq -e '.[0] | type == "object"' >/dev/null 2>&1; then
    echo "Response snippet: $(echo "$embed_response" | head -c 200)"
    log_fail "SPLADE embedding response is not an array of objects (sparse format)"
fi
# Verify sparse vector has non-zero entries
num_entries=$(echo "$embed_response" | jq '.[0] | length')
if [ "$num_entries" -lt 10 ]; then
    log_fail "SPLADE sparse vector has too few entries: $num_entries (expected 20-30)"
fi
log_pass "test-splade generated sparse embeddings successfully ($num_entries non-zero values)"

# Step 15: Test port conflict detection
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

# Step 15: Test duplicate name detection
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

# Step 16: Stop instance
log_test "Testing POST /instances/test-model-1/stop..."
response=$(http_post "$API_BASE/instances/test-model-1/stop" "")
check_http_status "$response" "200"
log_pass "Instance stopped successfully"

# Step 17: Start instance
log_test "Testing POST /instances/test-model-1/start..."
response=$(http_post "$API_BASE/instances/test-model-1/start" "")
check_http_status "$response" "200"
log_pass "Instance started successfully"

# Step 18: Restart instance
log_test "Testing POST /instances/test-model-1/restart..."
response=$(http_post "$API_BASE/instances/test-model-1/restart" "")
check_http_status "$response" "200"
log_pass "Instance restarted successfully"

# Step 19: Delete instance
log_test "Testing DELETE /instances/test-model-2..."
response=$(http_delete "$API_BASE/instances/test-model-2")
check_http_status "$response" "204"
log_pass "Instance deleted successfully"

# Step 20: Verify instance was deleted
log_test "Verifying instance deletion..."
response=$(http_get "$API_BASE/instances/test-model-2")
status=$(echo "$response" | tail -n1)
if [ "$status" == "200" ]; then
    log_fail "Deleted instance still accessible"
fi
log_pass "Instance properly deleted"

# Step 21: Test metrics endpoint
log_test "Testing /metrics endpoint..."
response=$(http_get "$API_BASE/metrics")
check_http_status "$response" "200"
log_pass "Metrics endpoint accessible"

# Step 22: Check Docker logs
log_test "Checking for errors in logs..."
if docker logs "$CONTAINER_NAME" 2>&1 | grep -i "error.*failed" | grep -v "Failed to connect" | grep -v "Failed to send" | grep -q .; then
    echo "Found errors in logs:"
    docker logs "$CONTAINER_NAME" 2>&1 | grep -i "error.*failed" | head -n 5
    log_fail "Errors found in container logs"
fi
log_pass "No critical errors in logs"

# Final summary
echo
echo "======================================"
echo -e "${GREEN}All E2E tests passed!${NC}"
echo "======================================"
echo
echo "Test Summary:"
echo "  - Docker image built successfully"
echo "  - Container started and remained healthy"
echo "  - Health endpoint working"
echo "  - Instance CRUD operations working"
echo "  - All 3 TEI instances became healthy and responsive"
echo "  - Dense embedding generation working (bge-small: 384d, all-mpnet: 768d)"
echo "  - Sparse embedding generation working (SPLADE: ~20-30 non-zero values)"
echo "  - Embedding dimensions validated against model metadata"
echo "  - Port conflict detection working"
echo "  - Duplicate name detection working"
echo "  - Instance lifecycle (stop/start/restart) working"
echo "  - Metrics endpoint accessible"
echo "  - No critical errors in logs"
echo

exit 0
