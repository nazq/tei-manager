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

# Setup test environment (no longer needed - tests use testcontainers)
setup-test:
    @echo "No setup required - tests use testcontainers or /bin/sleep stub"

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

# Generate main branch coverage baseline for patch comparison
cov-main:
    #!/usr/bin/env bash
    set -euo pipefail

    CURRENT_BRANCH=$(git branch --show-current)

    # Get main branch commit hash
    MAIN_HASH=$(git rev-parse origin/main)
    LCOV_FILE="main_${MAIN_HASH:0:12}.lcov"

    # Check if we already have coverage for this commit
    if [ -f "$LCOV_FILE" ]; then
        echo "Coverage for main ($MAIN_HASH) already exists: $LCOV_FILE"
        ln -sf "$LCOV_FILE" main.lcov
        echo "Symlinked main.lcov -> $LCOV_FILE"
        exit 0
    fi

    # Clean up old main_*.lcov files
    for old_lcov in main_*.lcov; do
        if [ -f "$old_lcov" ] && [ "$old_lcov" != "$LCOV_FILE" ]; then
            echo "Removing old coverage file: $old_lcov"
            rm -f "$old_lcov"
        fi
    done

    # Stash any uncommitted changes
    STASHED=false
    if [ -n "$(git status --porcelain)" ]; then
        echo "Stashing uncommitted changes..."
        git stash push -m "cov-main temp stash"
        STASHED=true
    fi

    # Switch to main and generate coverage
    echo "Switching to main branch..."
    git checkout main

    echo "Generating main branch coverage for $MAIN_HASH..."
    # Only run lib tests on main to avoid issues with tests that don't exist on main
    cargo llvm-cov --lib --lcov --output-path "$LCOV_FILE"

    # Switch back
    echo "Switching back to $CURRENT_BRANCH..."
    git checkout "$CURRENT_BRANCH"

    if [ "$STASHED" = true ]; then
        echo "Restoring stashed changes..."
        git stash pop
    fi

    # Create symlink for convenience
    ln -sf "$LCOV_FILE" main.lcov

    echo ""
    echo "Main branch coverage saved to $LCOV_FILE (symlinked as main.lcov)"
    echo "Run 'just cov-patch' to compare patch coverage"

# Generate patch coverage comparison against main.lcov
cov-patch:
    #!/usr/bin/env bash
    set -euo pipefail

    if [ ! -f main.lcov ]; then
        echo "main.lcov not found. Run 'just cov-main' first."
        exit 1
    fi

    echo "Generating current branch coverage..."
    # Run all tests but only measure library code coverage
    cargo llvm-cov --ignore-filename-regex='tests/.*' --lcov --output-path current.lcov

    echo ""
    echo "Comparing coverage..."
    echo ""

    # Extract line coverage percentages
    MAIN_LINES=$(grep -oP 'LF:\K\d+' main.lcov | paste -sd+ | bc)
    MAIN_HIT=$(grep -oP 'LH:\K\d+' main.lcov | paste -sd+ | bc)
    MAIN_PCT=$(echo "scale=2; $MAIN_HIT * 100 / $MAIN_LINES" | bc)

    CURR_LINES=$(grep -oP 'LF:\K\d+' current.lcov | paste -sd+ | bc)
    CURR_HIT=$(grep -oP 'LH:\K\d+' current.lcov | paste -sd+ | bc)
    CURR_PCT=$(echo "scale=2; $CURR_HIT * 100 / $CURR_LINES" | bc)

    DIFF=$(echo "scale=2; $CURR_PCT - $MAIN_PCT" | bc)

    # Calculate new lines added
    NEW_LINES=$((CURR_LINES - MAIN_LINES))
    NEW_HIT=$((CURR_HIT - MAIN_HIT))
    if [ "$NEW_LINES" -gt 0 ]; then
        NEW_PCT=$(echo "scale=2; $NEW_HIT * 100 / $NEW_LINES" | bc)
    else
        NEW_PCT="N/A"
    fi

    echo "Main branch:    $MAIN_PCT% ($MAIN_HIT/$MAIN_LINES lines)"
    echo "Current branch: $CURR_PCT% ($CURR_HIT/$CURR_LINES lines)"
    echo "Difference:     $DIFF%"
    echo ""
    if [ "$NEW_LINES" -gt 0 ]; then
        echo "New code:       $NEW_PCT% ($NEW_HIT/$NEW_LINES new lines covered)"
    fi
    echo ""

    # Check if coverage dropped significantly
    # Allow some drop when adding substantial new code (>5% of codebase)
    NEW_CODE_RATIO=$(echo "scale=2; $NEW_LINES * 100 / $MAIN_LINES" | bc)
    if (( $(echo "$DIFF < -5" | bc -l) )); then
        echo "⚠️  Coverage dropped significantly (>5%)!"
        exit 1
    elif (( $(echo "$DIFF < -2" | bc -l) )) && (( $(echo "$NEW_CODE_RATIO < 10" | bc -l) )); then
        echo "⚠️  Coverage dropped by more than 2% without adding much new code"
        exit 1
    elif (( $(echo "$DIFF < 0" | bc -l) )); then
        echo "⚠️  Coverage slightly decreased (acceptable with new code)"
    else
        echo "✅ Coverage maintained or improved"
    fi

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

# Run local benchmarks (no external dependencies)
bench-local:
    cargo bench --bench embedding
    cargo bench --bench pool
    cargo bench --bench registry

# Benchmark performance (requires bench-start first for multiplexer_overhead)
bench:
    cargo bench

# Benchmark and open HTML report in browser
bench-open:
    cargo bench -- --open

# Start local benchmark environment (tei-manager + bench-instance)
bench-start:
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

    # Find TEI binary - check common locations
    TEI_BINARY=""
    for candidate in \
        "${TEI_BINARY_PATH:-}" \
        "$(which text-embeddings-router 2>/dev/null || true)" \
        "$HOME/.local/bin/text-embeddings-router" \
        "/usr/local/bin/text-embeddings-router"; do
        if [ -n "$candidate" ] && [ -x "$candidate" ]; then
            TEI_BINARY="$candidate"
            break
        fi
    done

    if [ -z "$TEI_BINARY" ]; then
        echo "ERROR: TEI binary not found"
        echo "Install text-embeddings-router or set TEI_BINARY_PATH"
        echo "Download from: https://github.com/huggingface/text-embeddings-inference/releases"
        exit 1
    fi
    echo "Using TEI binary: $TEI_BINARY"

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

# Release workflow info (releases are automated via release-please)
release-info:
    @echo "Releases are automated via release-please"
    @echo ""
    @echo "Workflow:"
    @echo "  1. Merge PRs with conventional commits (feat:, fix:, etc.) to main"
    @echo "  2. Release-please automatically creates/updates a Release PR"
    @echo "  3. Review the Release PR, then update README versions:"
    @echo "     gh pr checkout <release-pr-number>"
    @echo "     ./scripts/update-readme-version.sh --commit"
    @echo "     git push"
    @echo "  4. Merge the Release PR to trigger Docker builds and GitHub release"
    @echo ""
    @echo "See CONTRIBUTING.md for details."

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
validate: clean dev-setup fcheck coverage
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

# Save benchmark baseline (run on main branch before changes)
bench-baseline:
    #!/usr/bin/env bash
    set -euo pipefail

    echo "Saving benchmark baseline..."
    BASELINE_DIR=".bench-baseline"
    mkdir -p "$BASELINE_DIR"

    # Get current commit hash
    COMMIT=$(git rev-parse --short HEAD)
    echo "$COMMIT" > "$BASELINE_DIR/commit"

    # Run benchmarks and save results
    cargo bench -- --save-baseline main
    echo "Baseline saved for commit $COMMIT"
    echo "Run 'just bench-compare' after making changes to compare"

# Compare benchmarks against baseline
bench-compare:
    #!/usr/bin/env bash
    set -euo pipefail

    BASELINE_DIR=".bench-baseline"

    if [ ! -f "$BASELINE_DIR/commit" ]; then
        echo "No baseline found. Run 'just bench-baseline' first on main branch."
        exit 1
    fi

    BASELINE_COMMIT=$(cat "$BASELINE_DIR/commit")
    echo "Comparing against baseline from commit $BASELINE_COMMIT"
    echo ""

    # Run benchmarks and compare
    cargo bench -- --baseline main

    echo ""
    echo "If you see significant regressions (>10%), investigate before merging."

# Full benchmark regression check (saves baseline, makes comparison)
bench-regression:
    #!/usr/bin/env bash
    set -euo pipefail

    echo "=== Benchmark Regression Detection ==="
    echo ""

    # Check if we have uncommitted changes
    if [ -n "$(git status --porcelain)" ]; then
        echo "Warning: You have uncommitted changes"
        echo ""
    fi

    # Get current branch
    CURRENT_BRANCH=$(git branch --show-current)
    BASELINE_DIR=".bench-baseline"

    # If baseline doesn't exist or is old, create it from main
    if [ ! -f "$BASELINE_DIR/commit" ]; then
        echo "No baseline found. Creating baseline from main branch..."
        echo ""

        # Stash current changes if any
        STASHED=false
        if [ -n "$(git status --porcelain)" ]; then
            git stash push -m "bench-regression temp stash"
            STASHED=true
        fi

        # Switch to main, build, and run baseline
        git checkout main
        cargo build --release
        cargo bench -- --save-baseline main
        git rev-parse --short HEAD > "$BASELINE_DIR/commit"

        # Switch back
        git checkout "$CURRENT_BRANCH"
        if [ "$STASHED" = true ]; then
            git stash pop
        fi

        echo ""
        echo "Baseline created. Now running comparison..."
        echo ""
    fi

    # Build current branch
    cargo build --release

    # Run comparison
    cargo bench -- --baseline main

    echo ""
    echo "=== Summary ==="
    echo "Baseline commit: $(cat $BASELINE_DIR/commit)"
    echo "Current branch: $CURRENT_BRANCH"
    echo ""
    echo "Look for 'Performance has regressed' warnings above."
    echo "Regressions >10% should be investigated."

# Quick benchmark (run without saving)
bench-quick:
    cargo bench -- --warm-up-time 1 --measurement-time 3

# List benchmark targets
bench-list:
    cargo bench -- --list

# Clean benchmark data
bench-clean:
    rm -rf target/criterion
    rm -rf .bench-baseline
    echo "Benchmark data cleaned"

# Install cargo-deny for dependency checks
install-deny:
    cargo install cargo-deny

# Run cargo-deny checks
deny:
    cargo deny check
