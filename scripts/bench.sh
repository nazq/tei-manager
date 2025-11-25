#!/bin/bash
# TEI Manager Benchmark Client
# Works locally and inside Docker container
set -e

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

log_info() { echo -e "${BLUE}➜${NC} $1"; }
log_success() { echo -e "${GREEN}✓${NC} $1"; }
log_warn() { echo -e "${YELLOW}⚠${NC} $1"; }
log_error() { echo -e "${RED}✗${NC} $1"; }

# =============================================================================
# Auto-detect environment and config
# =============================================================================

detect_environment() {
    # Detect if running in Docker or locally
    if [ -f "/.dockerenv" ] || grep -q docker /proc/1/cgroup 2>/dev/null; then
        ENV_TYPE="docker"
        DEFAULT_CONFIG="/etc/tei-manager/config/tei-manager.toml"
        DEFAULT_CERT_DIR="/etc/tei-manager/certs"
    else
        ENV_TYPE="local"
        # Try to find project root
        if [ -f "config/tei-manager.toml" ]; then
            DEFAULT_CONFIG="config/tei-manager.toml"
        else
            DEFAULT_CONFIG=""
        fi
        DEFAULT_CERT_DIR="certs"
    fi

    # Allow env var override
    CONFIG_FILE="${TEI_CONFIG:-$DEFAULT_CONFIG}"
    CERT_DIR="${TEI_CERT_DIR:-$DEFAULT_CERT_DIR}"
}

# Parse TOML value (simple parser for key = value or key = "value")
parse_toml_value() {
    local file="$1"
    local key="$2"
    local section="${3:-}"

    if [ -n "$section" ]; then
        # Extract value from within a section
        awk -v section="$section" -v key="$key" '
            /^\[/ { in_section = ($0 ~ "\\[" section "\\]") }
            in_section && $1 == key && $2 == "=" {
                gsub(/^[^=]*= */, "")
                gsub(/^"/, "")
                gsub(/"$/, "")
                gsub(/^'\''/, "")
                gsub(/'\''$/, "")
                print
                exit
            }
        ' "$file"
    else
        # Extract top-level value
        awk -v key="$key" '
            /^\[/ { in_section = 1 }
            !in_section && $1 == key && $2 == "=" {
                gsub(/^[^=]*= */, "")
                gsub(/^"/, "")
                gsub(/"$/, "")
                gsub(/^'\''/, "")
                gsub(/'\''$/, "")
                print
                exit
            }
            BEGIN { in_section = 0 }
        ' "$file"
    fi
}

# Load configuration from TOML
load_config() {
    if [ -z "$CONFIG_FILE" ] || [ ! -f "$CONFIG_FILE" ]; then
        log_error "Config file not found: $CONFIG_FILE"
        log_error "Set TEI_CONFIG env var or run from project root"
        exit 1
    fi

    log_info "Loading config from: $CONFIG_FILE"

    # Parse ports
    API_PORT=$(parse_toml_value "$CONFIG_FILE" "api_port")
    GRPC_PORT=$(parse_toml_value "$CONFIG_FILE" "grpc_port")
    GRPC_ENABLED=$(parse_toml_value "$CONFIG_FILE" "grpc_enabled")

    # Parse auth settings
    AUTH_ENABLED=$(parse_toml_value "$CONFIG_FILE" "enabled" "auth")

    # Defaults
    API_PORT="${API_PORT:-9000}"
    GRPC_PORT="${GRPC_PORT:-9001}"
    GRPC_ENABLED="${GRPC_ENABLED:-true}"
    AUTH_ENABLED="${AUTH_ENABLED:-false}"

    # Determine protocol based on auth
    if [ "$AUTH_ENABLED" = "true" ]; then
        PROTOCOL="https"
    else
        PROTOCOL="http"
    fi

    # Build endpoints (allow override via env vars)
    API_HOST="${TEI_HOST:-localhost}"
    API_ENDPOINT="${TEI_API_ENDPOINT:-${PROTOCOL}://${API_HOST}:${API_PORT}}"
    GRPC_ENDPOINT="${TEI_GRPC_ENDPOINT:-${PROTOCOL}://${API_HOST}:${GRPC_PORT}}"

    # Cert paths (only needed if auth enabled)
    CERT_PATH="${TEI_CERT:-${CERT_DIR}/client.pem}"
    KEY_PATH="${TEI_KEY:-${CERT_DIR}/client-key.pem}"
    CA_PATH="${TEI_CA:-${CERT_DIR}/ca.pem}"

    log_success "Environment: $ENV_TYPE"
    log_success "API endpoint: $API_ENDPOINT"
    log_success "gRPC endpoint: $GRPC_ENDPOINT"
    if [ "$AUTH_ENABLED" = "true" ]; then
        log_success "Auth: mTLS enabled"
    else
        log_success "Auth: disabled (plain HTTP/gRPC)"
    fi
}

# =============================================================================
# Find binaries/scripts
# =============================================================================

find_binary() {
    local binary_name=$1

    # Check if in PATH (installed location like /usr/local/bin/)
    if command -v "$binary_name" > /dev/null 2>&1; then
        command -v "$binary_name"
        return 0
    fi

    # Check local dev path
    if [ -f "target/release/$binary_name" ]; then
        echo "./target/release/$binary_name"
        return 0
    fi

    return 1
}

find_raw_gpu_script() {
    # Check if in PATH (installed location)
    if command -v raw-gpu-test.py > /dev/null 2>&1; then
        command -v raw-gpu-test.py
        return 0
    fi

    # Check local dev path
    if [ -f "scripts/raw-gpu-test.py" ]; then
        echo "scripts/raw-gpu-test.py"
        return 0
    fi

    return 1
}

# =============================================================================
# Commands
# =============================================================================

usage() {
    cat << EOF
Usage: $0 <command> [options]

Commands:
  status                              - Check TEI Manager status and instances
  bench <instance> [texts] [batch]    - Run both benchmarks and compare (Arrow vs Raw GPU)
  bench-arrow <instance> [texts] [batch] - Run Arrow benchmark only (via bench-client)
  bench-raw <instance> [texts] [batch]   - Run Raw GPU benchmark only (direct PyTorch)

Options:
  instance   - Instance name to benchmark
  texts      - Number of texts (default: 10000)
  batch      - Batch size (default: 1000)

Environment Variables:
  TEI_CONFIG        - Config file path (auto-detected)
  TEI_HOST          - Host address (default: localhost)
  TEI_API_ENDPOINT  - Override API endpoint
  TEI_GRPC_ENDPOINT - Override gRPC endpoint
  TEI_CERT_DIR      - Certificate directory
  TEI_CERT          - Client certificate path
  TEI_KEY           - Client key path
  TEI_CA            - CA certificate path

Examples:
  $0 status
  $0 bench my-instance 10000 1000       # Compare Arrow vs Raw GPU
  $0 bench-arrow my-instance 50000 1000 # Arrow only
  $0 bench-raw my-instance 50000 1000   # Raw GPU only
EOF
    exit 1
}

cmd_status() {
    log_info "TEI Manager Status"
    echo ""

    # Build curl args
    local curl_args="-s"
    if [ "$AUTH_ENABLED" = "true" ]; then
        curl_args="$curl_args -k --cert $CERT_PATH --key $KEY_PATH"
    fi

    # Health check
    if curl $curl_args "$API_ENDPOINT/health" > /dev/null 2>&1; then
        log_success "API is healthy"
    else
        log_error "API is not responding at $API_ENDPOINT"
        return 1
    fi

    # List instances
    echo ""
    log_info "Instances:"
    curl $curl_args "$API_ENDPOINT/instances" 2>/dev/null | jq '.' || echo "  Failed to fetch instances"
}

cmd_bench_arrow() {
    local instance="${1:-}"
    local num_texts="${2:-10000}"
    local batch_size="${3:-1000}"

    if [ -z "$instance" ]; then
        log_error "Instance name required"
        usage
    fi

    log_info "Arrow benchmark: instance=$instance, texts=$num_texts, batch_size=$batch_size"

    # Find bench-client binary
    if ! BENCH_BIN=$(find_binary "bench-client"); then
        log_error "bench-client binary not found"
        log_error "  - Docker: Should be at /usr/local/bin/bench-client"
        log_error "  - Local: Run 'cargo build --release --bin bench-client'"
        exit 1
    fi

    # Build args
    local args="--endpoint $GRPC_ENDPOINT --instance $instance --mode arrow"
    args="$args --num-texts $num_texts --batch-size $batch_size"

    if [ "$AUTH_ENABLED" = "true" ]; then
        args="$args --cert $CERT_PATH --key $KEY_PATH --ca $CA_PATH --insecure"
    fi

    $BENCH_BIN $args
}

cmd_bench_raw() {
    local instance="${1:-}"
    local num_texts="${2:-10000}"
    local batch_size="${3:-}"

    if [ -z "$instance" ]; then
        log_error "Instance name required"
        usage
    fi

    # Find raw-gpu-test.py script
    if ! RAW_GPU_SCRIPT=$(find_raw_gpu_script); then
        log_error "raw-gpu-test.py not found"
        log_error "  - Docker: Should be at /usr/local/bin/raw-gpu-test.py"
        log_error "  - Local: Should be at scripts/raw-gpu-test.py"
        exit 1
    fi

    if [ -z "$batch_size" ]; then
        log_info "Raw GPU benchmark: instance=$instance, texts=$num_texts (single batch)"
    else
        log_info "Raw GPU benchmark: instance=$instance, texts=$num_texts, batch_size=$batch_size"
    fi

    # Build args for raw-gpu-test.py
    local args="$instance $num_texts"
    if [ -n "$batch_size" ]; then
        args="$args $batch_size"
    fi

    if [ "$AUTH_ENABLED" = "true" ]; then
        args="$args --tei-endpoint $GRPC_ENDPOINT --cert $CERT_PATH --key $KEY_PATH"
    else
        args="$args --tei-endpoint $API_ENDPOINT"
    fi

    uv run "$RAW_GPU_SCRIPT" $args
}

cmd_bench_compare() {
    local instance="${1:-}"
    local num_texts="${2:-10000}"
    local batch_size="${3:-1000}"

    if [ -z "$instance" ]; then
        log_error "Instance name required"
        usage
    fi

    log_info "Comparative benchmark: instance=$instance, texts=$num_texts, batch_size=$batch_size"
    echo ""

    # Find bench-client binary
    if ! BENCH_BIN=$(find_binary "bench-client"); then
        log_error "bench-client binary not found"
        exit 1
    fi

    # Find raw-gpu-test.py script
    if ! RAW_GPU_SCRIPT=$(find_raw_gpu_script); then
        log_error "raw-gpu-test.py not found"
        exit 1
    fi

    # ==========================================================================
    # Run Arrow benchmark
    # ==========================================================================
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}  Arrow Benchmark (TEI Manager + gRPC)${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local bench_args="--endpoint $GRPC_ENDPOINT --instance $instance --mode arrow"
    bench_args="$bench_args --num-texts $num_texts --batch-size $batch_size"

    if [ "$AUTH_ENABLED" = "true" ]; then
        bench_args="$bench_args --cert $CERT_PATH --key $KEY_PATH --ca $CA_PATH --insecure"
    fi

    ARROW_OUTPUT=$($BENCH_BIN $bench_args 2>&1)
    ARROW_THROUGHPUT=$(echo "$ARROW_OUTPUT" | grep -oE '"throughput_per_sec": [0-9]+\.?[0-9]*' | grep -oE '[0-9]+\.?[0-9]*')
    ARROW_DURATION=$(echo "$ARROW_OUTPUT" | grep -oE '"total_duration_secs": [0-9]+\.?[0-9]*' | grep -oE '[0-9]+\.?[0-9]*')

    echo "$ARROW_OUTPUT" | tail -20
    echo ""

    # ==========================================================================
    # Run Raw GPU benchmark
    # ==========================================================================
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}  Raw GPU Benchmark (Direct PyTorch)${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local raw_args="$instance $num_texts $batch_size"
    if [ "$AUTH_ENABLED" = "true" ]; then
        raw_args="$raw_args --tei-endpoint $GRPC_ENDPOINT --cert $CERT_PATH --key $KEY_PATH"
    else
        raw_args="$raw_args --tei-endpoint $API_ENDPOINT"
    fi

    # Run raw benchmark and capture output + exit code
    set +e
    RAW_OUTPUT=$(uv run "$RAW_GPU_SCRIPT" $raw_args 2>&1)
    RAW_EXIT_CODE=$?
    set -e

    echo "$RAW_OUTPUT"
    echo ""

    # Check if benchmark succeeded
    if [ $RAW_EXIT_CODE -ne 0 ]; then
        log_error "Raw GPU benchmark failed with exit code $RAW_EXIT_CODE"
        echo ""
        log_error "Skipping performance comparison"
        return 1
    fi

    RAW_THROUGHPUT=$(echo "$RAW_OUTPUT" | grep "RESULT:" | grep -oE '[0-9,]+' | head -1 | tr -d ',')
    RAW_DURATION=$(echo "$RAW_OUTPUT" | grep "Duration:" | grep -oE '[0-9]+\.[0-9]+' | head -1)

    # ==========================================================================
    # Calculate comparison
    # ==========================================================================
    if [ -n "$ARROW_THROUGHPUT" ] && [ -n "$RAW_THROUGHPUT" ]; then
        ARROW_INT=$(printf "%.0f" "$ARROW_THROUGHPUT")
        RAW_INT=$(printf "%.0f" "$RAW_THROUGHPUT")

        DIFF=$((RAW_INT - ARROW_INT))
        if [ "$RAW_INT" -gt 0 ]; then
            PERCENT=$(awk "BEGIN {printf \"%.1f\", ($DIFF / $RAW_INT) * 100}")
        else
            PERCENT="0.0"
        fi

        echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
        echo -e "${GREEN}  Performance Comparison${NC}"
        echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
        printf "  %-25s %'10d emb/sec  (%.2fs)\n" "Arrow (TEI Manager):" "$ARROW_INT" "$ARROW_DURATION"
        printf "  %-25s %'10d emb/sec  (%.2fs)\n" "Raw GPU (PyTorch):" "$RAW_INT" "${RAW_DURATION:-0}"
        echo ""
        printf "  %-25s %'10d emb/sec  (%.1f%% overhead)\n" "Difference:" "$DIFF" "$PERCENT"
        echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    else
        log_error "Failed to extract throughput metrics"
    fi
}

# =============================================================================
# Main
# =============================================================================

detect_environment
load_config

ACTION="${1:-}"
shift || true

case "$ACTION" in
    status)
        cmd_status
        ;;
    bench)
        cmd_bench_compare "$@"
        ;;
    bench-arrow)
        cmd_bench_arrow "$@"
        ;;
    bench-raw)
        cmd_bench_raw "$@"
        ;;
    help|-h|--help|"")
        usage
        ;;
    *)
        log_error "Unknown command: $ACTION"
        usage
        ;;
esac
