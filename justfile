# TEI Manager - Development Commands
# Install just: cargo install just
# Run: just <target>

# Default recipe - show available commands
default:
    @just --list

# Format code with rustfmt
fmt:
    cargo fmt --all

# Check formatting without making changes
fmt-check:
    cargo fmt --all -- --check

# Run clippy linter
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Setup test environment (extract TEI binary)
setup-test:
    ./scripts/setup-test-binary.sh

# Run all unit tests
test:
    cargo test --lib

# Run unit tests with output
test-verbose:
    cargo test --lib -- --nocapture

# Run specific test
test-one TEST:
    cargo test --lib {{TEST}} -- --nocapture

# Run integration tests (requires setup-test first)
test-integration:
    cargo test --test integration

# Run all tests (unit + integration)
test-all: test test-integration
    @echo "✅ All tests passed!"

# Run all checks (fmt, clippy, test)
check: fmt-check clippy test-all
    @echo "✅ All checks passed!"

# Format then check (fmt + fmt-check + clippy + test)
fcheck: fmt check
    @echo "✅ Formatted and all checks passed!"

# Generate code coverage report
coverage:
    #!/usr/bin/env bash
    set -uxo pipefail

    # Install llvm-cov if not present
    if ! command -v cargo-llvm-cov &> /dev/null; then
        echo "Installing cargo-llvm-cov..."
        cargo install cargo-llvm-cov
    fi

    # Run coverage
    cargo llvm-cov --html --output-dir coverage --ignore-filename-regex='tests/.*' --open

# Generate coverage for CI (lcov format)
coverage-ci:
    cargo llvm-cov --lcov --output-path coverage/lcov.info --ignore-filename-regex='tests/.*'

# Build release binary
build:
    cargo build --release

# Build debug binary
build-debug:
    cargo build

# Clean build artifacts
clean:
    cargo clean
    rm -rf coverage/
    rm -rf target/


# Watch for changes and run tests
watch:
    cargo watch -x 'test --lib'

# Watch for changes and run specific test
watch-test TEST:
    cargo watch -x 'test --lib {{TEST}}'

# Watch and run on changes (with clippy)
watch-check:
    cargo watch -x clippy -x 'test --lib'

# Check dependencies for updates
deps-check:
    cargo outdated

# Update dependencies
deps-update:
    cargo update

# Audit dependencies for security vulnerabilities
deps-audit:
    cargo audit

# Install development tools
dev-setup:
    #!/usr/bin/env bash
    set -euxo pipefail

    echo "Installing development tools..."

    # Rustfmt and clippy (usually included with rustup)
    rustup component add rustfmt clippy

    # Install llvm-tools for coverage
    rustup component add llvm-tools-preview

    # Cargo tools
    cargo install cargo-watch || true
    cargo install cargo-llvm-cov || true
    cargo install cargo-outdated || true
    cargo install cargo-audit || true

    echo "✅ Development tools installed!"

# Run pre-commit checks (runs before committing)
pre-commit: fcheck
    @echo "✅ Ready to commit!"

# Full CI pipeline (what CI runs)
ci: fmt-check clippy test
    @echo "✅ CI pipeline passed!"

# Benchmark performance (requires bench-start first)
bench:
    cargo bench

# Benchmark and open HTML report in browser
bench-open:
    cargo bench -- --open

# Start local benchmark environment (tei-manager + bench-instance)
bench-start: setup-test
    #!/usr/bin/env bash
    set -euo pipefail

    # Check if already running
    if curl -s http://localhost:9000/health > /dev/null 2>&1; then
        echo "Benchmark environment already running!"
        just bench-status
        exit 0
    fi

    echo "Starting tei-manager for benchmarks..."

    # Build release binary
    cargo build --release

    # Get the TEI binary path (relative to justfile location)
    TEI_BINARY="$(pwd)/tests/text-embeddings-router"

    if [ ! -x "$TEI_BINARY" ]; then
        echo "ERROR: TEI binary not found at $TEI_BINARY"
        echo "Run: just setup-test"
        exit 1
    fi

    # Use a persistent state file in /tmp
    STATE_FILE="/tmp/tei-manager-bench.state"

    # Start tei-manager as daemon (nohup + redirect output)
    nohup env \
        TEI_MANAGER_STATE_FILE="$STATE_FILE" \
        TEI_MANAGER_API_PORT=9000 \
        TEI_MANAGER_GRPC_PORT=9001 \
        TEI_BINARY_PATH="$TEI_BINARY" \
        ./target/release/tei-manager > /tmp/tei-manager-bench.log 2>&1 &

    MANAGER_PID=$!
    echo $MANAGER_PID > /tmp/tei-manager-bench.pid
    echo "tei-manager started (PID: $MANAGER_PID)"

    # Wait for manager to be ready
    echo "Waiting for tei-manager..."
    for i in {1..30}; do
        if curl -s http://localhost:9000/health > /dev/null 2>&1; then
            echo "tei-manager is ready!"
            break
        fi
        sleep 1
    done

    # Create bench-instance on port 8081
    echo "Creating bench-instance..."
    curl -s -X POST http://localhost:9000/instances \
        -H "Content-Type: application/json" \
        -d '{"name": "bench-instance", "model_id": "BAAI/bge-small-en-v1.5", "port": 8081}' | jq .

    # Wait for instance to be running
    echo "Waiting for bench-instance to be ready..."
    for i in {1..120}; do
        STATUS=$(curl -s http://localhost:9000/instances/bench-instance | jq -r '.status // "unknown"')
        if [ "$STATUS" = "Running" ] || [ "$STATUS" = "running" ]; then
            echo "bench-instance is running!"
            break
        fi
        if [ "$STATUS" = "Failed" ] || [ "$STATUS" = "failed" ]; then
            echo "ERROR: bench-instance failed to start"
            echo "Check logs: cat /tmp/tei-manager-bench.log"
            exit 1
        fi
        echo "  Status: $STATUS (attempt $i/120)"
        sleep 2
    done

    echo ""
    echo "Benchmark environment ready!"
    echo "  - tei-manager API: http://localhost:9000"
    echo "  - gRPC multiplexer: http://localhost:9001"
    echo "  - TEI instance: http://localhost:8081"
    echo "  - Logs: /tmp/tei-manager-bench.log"
    echo ""
    echo "Run: just bench"
    echo "Stop: just bench-stop"

# Stop local benchmark environment
bench-stop:
    #!/usr/bin/env bash
    PID_FILE="/tmp/tei-manager-bench.pid"

    if [ ! -f "$PID_FILE" ]; then
        echo "No benchmark environment running (no PID file)"
        exit 0
    fi

    MANAGER_PID=$(cat "$PID_FILE")
    echo "Stopping benchmark environment (PID: $MANAGER_PID)..."

    # Get child processes (TEI instances) before killing parent
    CHILD_PIDS=$(pgrep -P "$MANAGER_PID" 2>/dev/null || true)

    # Kill tei-manager
    if kill -0 "$MANAGER_PID" 2>/dev/null; then
        kill "$MANAGER_PID"
        echo "  Stopped tei-manager"
    else
        echo "  tei-manager already stopped"
    fi

    # Kill any child processes (TEI instances)
    for pid in $CHILD_PIDS; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid"
            echo "  Stopped child process $pid"
        fi
    done

    # Cleanup
    rm -f "$PID_FILE"
    rm -f /tmp/tei-manager-bench.state
    echo "Done."

# Check benchmark environment status
bench-status:
    #!/usr/bin/env bash
    echo "=== Benchmark Environment Status ==="
    echo ""
    echo "tei-manager:"
    curl -s http://localhost:9000/health 2>/dev/null && echo " (healthy)" || echo "  Not running"
    echo ""
    echo "Instances:"
    curl -s http://localhost:9000/instances 2>/dev/null | jq -r '.[] | "  - \(.name): \(.status) (port \(.port))"' || echo "  None"
    echo ""
    echo "gRPC multiplexer:"
    grpcurl -plaintext localhost:9001 list 2>/dev/null && echo " (available)" || echo "  Not available"

# Generate documentation
docs:
    cargo doc --no-deps --open

# Check for unused dependencies
deps-unused:
    cargo +nightly udeps

# Expand macros (debugging)
expand:
    cargo expand

# Show crate info
info:
    @echo "=== Crate Information ==="
    @cargo --version
    @rustc --version
    @echo ""
    @echo "=== Project Dependencies ==="
    @cargo tree --depth 1
    @echo ""
    @echo "=== Binary Size ==="
    @ls -lh target/release/tei-manager 2>/dev/null || echo "Not built yet (run 'just build')"

# Release checklist
release VERSION:
    #!/usr/bin/env bash
    set -euxo pipefail

    echo "🚀 Release {{VERSION}} checklist:"
    echo ""

    # Check if working tree is clean
    if [ -n "$(git status --porcelain)" ]; then
        echo "❌ Working directory is not clean"
        exit 1
    fi

    # Update version in Cargo.toml
    sed -i 's/^version = ".*"/version = "{{VERSION}}"/' Cargo.toml

    # Run full checks
    just ci

    # Commit and tag
    git add Cargo.toml Cargo.lock
    git commit -m "chore: Release {{VERSION}}"
    git tag -a "v{{VERSION}}" -m "Release version {{VERSION}}"

    echo ""
    echo "✅ Release {{VERSION}} prepared!"
    echo ""
    echo "Next steps:"
    echo "  1. Review: git show HEAD"
    echo "  2. Push: git push origin main"
    echo "  3. Push tag: git push origin v{{VERSION}}"
    echo "  4. Run: ./release.sh {{VERSION}}"

# Quick fix - format and run clippy with auto-fix
fix:
    cargo fmt --all
    cargo clippy --all-targets --all-features --fix --allow-dirty --allow-staged

# Profile build time
profile-build:
    cargo clean
    cargo build --release --timings
    @echo "Build timing report: target/cargo-timings/cargo-timing.html"

# Check binary size
size:
    @echo "=== Binary Sizes ==="
    @ls -lh target/release/tei-manager 2>/dev/null || echo "Release binary not found (run 'just build')"
    @ls -lh target/debug/tei-manager 2>/dev/null || echo "Debug binary not found (run 'just build-debug')"
    @echo ""
    @echo "=== Stripped Size ==="
    @strip -s target/release/tei-manager -o target/release/tei-manager.stripped 2>/dev/null && ls -lh target/release/tei-manager.stripped || echo "Run 'just build' first"

# Lint Dockerfile
docker-lint:
    docker run --rm -i hadolint/hadolint < Dockerfile

# Security audit
security-audit:
    cargo audit
    @echo ""
    @echo "Checking for known vulnerabilities in dependencies..."

# Full local validation (everything before pushing)
validate: clean dev-setup fcheck e2e coverage
    @echo "✅ Full validation complete! Ready to push."

# Show project statistics
stats:
    @echo "=== Project Statistics ==="
    @echo ""
    @echo "Lines of code:"
    @tokei src/ 2>/dev/null || find src -name "*.rs" -exec wc -l {} + | tail -1
    @echo ""
    @echo "Dependencies:"
    @cargo tree --depth 0 | wc -l
    @echo ""
    @echo "Test coverage (run 'just coverage' first):"
    @grep -oP 'Coverage: \K[\d.]+' coverage/index.html 2>/dev/null || echo "No coverage data (run 'just coverage')"

# Run tests with nextest (faster test runner)
test-nextest:
    cargo nextest run

# Install nextest
install-nextest:
    cargo install cargo-nextest
