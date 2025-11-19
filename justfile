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

# Run all unit tests
test:
    cargo test --lib

# Run unit tests with output
test-verbose:
    cargo test --lib -- --nocapture

# Run specific test
test-one TEST:
    cargo test --lib {{TEST}} -- --nocapture

# Run all checks (fmt, clippy, test)
check: fmt-check clippy test
    @echo "✅ All checks passed!"

# Format then check (fmt + fmt-check + clippy + test)
fcheck: fmt check
    @echo "✅ Formatted and all checks passed!"

# Generate code coverage report
coverage:
    #!/usr/bin/env bash
    set -euxo pipefail

    # Install tarpaulin if not present
    if ! command -v cargo-tarpaulin &> /dev/null; then
        echo "Installing cargo-tarpaulin..."
        cargo install cargo-tarpaulin
    fi

    # Run coverage
    cargo tarpaulin \
        --out Html \
        --output-dir coverage \
        --exclude-files 'tests/*' 'target/*' \
        --timeout 300 \
        --verbose

    # Open HTML report
    echo "Opening coverage report..."
    if command -v xdg-open &> /dev/null; then
        xdg-open coverage/index.html
    elif command -v open &> /dev/null; then
        open coverage/index.html
    else
        echo "Coverage report generated at: coverage/index.html"
    fi

# Generate coverage and upload to codecov
coverage-ci:
    cargo tarpaulin \
        --out Xml \
        --output-dir coverage \
        --exclude-files 'tests/*' 'target/*' \
        --timeout 300

# Build release binary
build:
    cargo build --release

# Build debug binary
build-debug:
    cargo build

# Run the application in development mode
run *ARGS:
    cargo run -- {{ARGS}}

# Run with example config
run-example:
    cargo run -- --config config/tei-manager.example.toml --log-format pretty --log-level debug

# Clean build artifacts
clean:
    cargo clean
    rm -rf coverage/
    rm -rf target/

# Build Docker image
docker-build TAG="latest":
    docker build -t tei-manager:{{TAG}} .

# Build Docker image with no cache
docker-build-clean TAG="latest":
    docker build --no-cache -t tei-manager:{{TAG}} .

# Run E2E tests
e2e:
    ./test-e2e.sh

# Run E2E tests with Docker rebuild
e2e-clean:
    docker rmi tei-manager:test 2>/dev/null || true
    ./test-e2e.sh

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

    # Cargo tools
    cargo install cargo-watch || true
    cargo install cargo-tarpaulin || true
    cargo install cargo-outdated || true
    cargo install cargo-audit || true

    echo "✅ Development tools installed!"

# Run pre-commit checks (runs before committing)
pre-commit: fcheck
    @echo "✅ Ready to commit!"

# Full CI pipeline (what CI runs)
ci: fmt-check clippy test e2e
    @echo "✅ CI pipeline passed!"

# Benchmark performance
bench:
    cargo bench

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

# Run all tests including ignored ones
test-all:
    cargo test --lib -- --include-ignored

# Run tests with nextest (faster test runner)
test-nextest:
    cargo nextest run

# Install nextest
install-nextest:
    cargo install cargo-nextest
