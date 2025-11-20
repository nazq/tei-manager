#!/bin/bash
set -e

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${GREEN}=== Multiplexer Overhead Benchmark ===${NC}"
echo ""

# Configuration
INSTANCE_PORT=8081
MULTIPLEXER_PORT=9001  # Default gRPC port for tei-manager
MANAGER_PORT=3000
MODEL_ID=${MODEL_ID:-"BAAI/bge-small-en-v1.5"}
BENCH_INSTANCE="bench-instance"

# Check for GPU
if ! command -v nvidia-smi &> /dev/null; then
    echo -e "${RED}Error: nvidia-smi not found. This benchmark requires a GPU.${NC}"
    exit 1
fi

# Find text-embeddings-router binary
if command -v text-embeddings-router &> /dev/null; then
    TEI_BIN="text-embeddings-router"
elif [ -f "./tests/text-embeddings-router" ]; then
    TEI_BIN="./tests/text-embeddings-router"
else
    echo -e "${RED}Error: text-embeddings-router not found in PATH or ./tests/${NC}"
    echo "Please download it from https://github.com/huggingface/text-embeddings-inference"
    exit 1
fi
echo -e "${GREEN}Using TEI binary: ${TEI_BIN}${NC}"

echo -e "${YELLOW}GPU Info:${NC}"
nvidia-smi --query-gpu=name,memory.total --format=csv,noheader
echo ""

# Cleanup function
cleanup() {
    echo -e "${YELLOW}Cleaning up...${NC}"
    pkill -f "text-embeddings-router" || true
    pkill -f "tei-manager" || true
    sleep 2
}

trap cleanup EXIT

# Clean up any existing processes
cleanup

# Step 1: Build tei-manager
echo -e "${GREEN}Building tei-manager...${NC}"
cargo build --release

# Step 2: Start tei-manager (grpc_port defaults to 9001)
echo -e "${GREEN}Starting tei-manager on port ${MANAGER_PORT}...${NC}"
TEI_MANAGER_STATE_FILE=/tmp/bench-tei-manager-state.toml \
TEI_BINARY_PATH="${TEI_BIN}" \
./target/release/tei-manager \
    --port ${MANAGER_PORT} &
MANAGER_PID=$!

# Wait for manager to be ready
echo -e "${YELLOW}Waiting for tei-manager to be ready...${NC}"
for i in {1..30}; do
    if curl -s http://localhost:${MANAGER_PORT}/health >/dev/null 2>&1; then
        echo -e "${GREEN}tei-manager is ready!${NC}"
        break
    fi
    if [ $i -eq 30 ]; then
        echo -e "${RED}tei-manager failed to start${NC}"
        exit 1
    fi
    sleep 1
done

# Step 3: Create benchmark instance via API (this automatically starts it)
echo -e "${GREEN}Creating benchmark instance...${NC}"
curl -s -X POST http://localhost:${MANAGER_PORT}/instances \
    -H "Content-Type: application/json" \
    -d "{
        \"name\": \"${BENCH_INSTANCE}\",
        \"model_id\": \"${MODEL_ID}\",
        \"port\": ${INSTANCE_PORT},
        \"max_batch_tokens\": 16384,
        \"max_concurrent_requests\": 512,
        \"gpu_id\": 0
    }" | jq '.'

# Wait for benchmark instance TEI to be ready
# Note: Health checks have a 60s delay, so we check the TEI Prometheus endpoint directly
echo -e "${YELLOW}Waiting for benchmark instance TEI to be ready...${NC}"
for i in {1..30}; do
    if curl -s http://localhost:9100/metrics >/dev/null 2>&1; then
        echo -e "${GREEN}Benchmark instance TEI is ready!${NC}"
        break
    fi
    if [ $i -eq 30 ]; then
        echo -e "${RED}Benchmark instance TEI failed to start${NC}"
        exit 1
    fi
    sleep 1
done

echo ""
echo -e "${GREEN}Setup complete! Running comprehensive benchmark suite...${NC}"
echo ""
echo -e "${YELLOW}Configuration:${NC}"
echo "  Model: ${MODEL_ID}"
echo "  Direct Instance: grpc://localhost:${INSTANCE_PORT}"
echo "  Multiplexer: grpc://localhost:${MULTIPLEXER_PORT}"
echo "  Instance Name: ${BENCH_INSTANCE}"
echo ""
echo -e "${YELLOW}Benchmark Scenarios:${NC}"
echo "  1. Embedding overhead (short, medium, long, extra-long)"
echo "  2. Concurrent requests (5, 10, 20 parallel)"
echo ""

# Step 4: Run the benchmark
cargo bench --bench multiplexer_overhead

echo ""
echo -e "${GREEN}Benchmark complete!${NC}"
echo -e "${YELLOW}Results saved to target/criterion/${NC}"
