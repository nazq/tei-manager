#!/usr/bin/env bash
# ============================================================================
# DEPRECATED: Use GitHub Actions release workflow instead
#
# This script is kept for emergency/fallback use only.
# Preferred method: git tag v0.9.0 && git push --tags
#
# See: .github/workflows/release.yml
# ============================================================================

# ============================================================================
# Release script for TEI Manager
# ============================================================================
#
# Orchestrates the full release process:
#   1. Validates versions and working directory
#   2. Runs tests and quality checks
#   3. Builds all Docker image variants (via build-all-variants.sh)
#   4. Optionally pushes images to registry and creates git tags
#
# Usage:
#   ./release.sh <manager-version> <tei-version>           # Dry run (build only, no push/tag)
#   ./release.sh <manager-version> <tei-version> --release # Build, push images, create git tag
#
# Example:
#   ./release.sh 0.3.0 1.8.3              # Build images locally (dry run)
#   ./release.sh 0.3.0 1.8.3 --release    # Full release: build, push, tag
#
# ============================================================================

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Helper functions
log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check arguments
if [ $# -lt 2 ]; then
    log_error "Missing required arguments"
    echo "Usage: $0 <manager-version> <tei-version> [--release]"
    echo ""
    echo "Modes:"
    echo "  (default)   Dry run - build images locally, no push or git tags"
    echo "  --release   Full release - build, push to registry, create git tag"
    echo ""
    echo "Example: $0 0.3.0 1.8.3"
    echo "Example: $0 0.3.0 1.8.3 --release"
    exit 1
fi

MANAGER_VERSION=$1
TEI_VERSION=$2
MODE="${3:-}"

# Validate mode flag
DRY_RUN=true
if [ "$MODE" = "--release" ]; then
    DRY_RUN=false
elif [ -n "$MODE" ]; then
    log_error "Invalid flag: $MODE"
    echo "Valid flags: --release"
    exit 1
fi

# Validate version formats (semantic versioning)
if ! [[ "$MANAGER_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    log_error "Invalid manager version format: $MANAGER_VERSION"
    echo "Version must follow semantic versioning: MAJOR.MINOR.PATCH"
    echo "Example: 0.3.0"
    exit 1
fi

if ! [[ "$TEI_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    log_error "Invalid TEI version format: $TEI_VERSION"
    echo "Version must follow semantic versioning: MAJOR.MINOR.PATCH"
    echo "Example: 1.8.3"
    exit 1
fi

log_info "Starting release process"
log_info "TEI Manager version: $MANAGER_VERSION"
log_info "TEI version: $TEI_VERSION"
if [ "$DRY_RUN" = true ]; then
    log_warn "DRY RUN - will build images but not push or create git tags"
else
    log_info "FULL RELEASE - will build, push, and create git tags"
fi
echo ""

# Check if working directory is clean
if [ -n "$(git status --porcelain)" ]; then
    log_warn "Working directory is not clean"
    git status --short
    echo ""
    read -p "Continue anyway? (y/N) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        log_info "Release cancelled"
        exit 1
    fi
fi

# Verify Cargo.toml version matches
CARGO_VERSION=$(grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2)
if [ "$CARGO_VERSION" != "$MANAGER_VERSION" ]; then
    log_warn "Cargo.toml version ($CARGO_VERSION) doesn't match release version ($MANAGER_VERSION)"
    read -p "Update Cargo.toml version to $MANAGER_VERSION? (y/N) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        sed -i "s/^version = \".*\"/version = \"$MANAGER_VERSION\"/" Cargo.toml
        log_success "Updated Cargo.toml version to $MANAGER_VERSION"
        # Update Cargo.lock as well
        cargo update -p tei-manager
        log_success "Updated Cargo.lock"
    else
        log_error "Version mismatch - please update Cargo.toml manually"
        exit 1
    fi
fi

# Check if current version is in changelog
if ! grep -q "\[$MANAGER_VERSION\]" CHANGELOG.md; then
    log_warn "Version $MANAGER_VERSION not found in CHANGELOG.md"
    echo "Please update CHANGELOG.md with release notes for v$MANAGER_VERSION"
    echo "Press any key to continue after updating..."
    read -n 1 -s
fi

# Run all checks (fmt, clippy, test)
log_info "Running checks (just check)..."
if just check; then
    log_success "All checks passed"
else
    log_error "Checks failed"
    exit 1
fi

# Build all Docker image variants using build-all-variants.sh
log_info "Building all Docker image variants..."
echo ""

BUILD_SCRIPT="./scripts/build-all-variants.sh"

if [ ! -f "$BUILD_SCRIPT" ]; then
    log_error "Build script not found: $BUILD_SCRIPT"
    exit 1
fi

if [ ! -x "$BUILD_SCRIPT" ]; then
    log_warn "Build script is not executable, fixing..."
    chmod +x "$BUILD_SCRIPT"
fi

# Run the build script with appropriate arguments
# In dry-run mode, build locally only. In release mode, build and push.
if [ "$DRY_RUN" = true ]; then
    log_info "Executing: $BUILD_SCRIPT $MANAGER_VERSION $TEI_VERSION"
    if $BUILD_SCRIPT "$MANAGER_VERSION" "$TEI_VERSION"; then
        log_success "All Docker variants built successfully"
    else
        log_error "Docker build failed"
        exit 1
    fi
else
    log_info "Executing: $BUILD_SCRIPT $MANAGER_VERSION $TEI_VERSION --push"
    if $BUILD_SCRIPT "$MANAGER_VERSION" "$TEI_VERSION" --push; then
        log_success "All Docker variants built and pushed successfully"
    else
        log_error "Docker build/push failed"
        exit 1
    fi
fi

echo ""

# If dry run, we're done here
if [ "$DRY_RUN" = true ]; then
    log_success "Dry run complete - images built locally"
    echo ""
    echo "To perform a full release, run:"
    echo "  $0 $MANAGER_VERSION $TEI_VERSION --release"
    echo ""
    echo "This will:"
    echo "  - Push images to ghcr.io"
    echo "  - Create git tag v${MANAGER_VERSION}"
    echo "  - Create GitHub release with changelog notes"
    echo ""
    exit 0
fi

# Create git tag
GIT_TAG="v${MANAGER_VERSION}"
log_info "Creating git tag ${GIT_TAG}..."

# Check if tag already exists
if git rev-parse "$GIT_TAG" >/dev/null 2>&1; then
    log_warn "Tag $GIT_TAG already exists"
    read -p "Overwrite existing tag? (y/N) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        log_info "Keeping existing tag"
    else
        git tag -d "$GIT_TAG"
        git tag -a "$GIT_TAG" -m "Release version ${MANAGER_VERSION} with TEI ${TEI_VERSION}"
        log_success "Updated git tag $GIT_TAG"
    fi
else
    git tag -a "$GIT_TAG" -m "Release version ${MANAGER_VERSION} with TEI ${TEI_VERSION}"
    log_success "Created git tag $GIT_TAG"
fi

# Push git tag
log_info "Pushing git tag to origin..."
if git push origin "$GIT_TAG" --force; then
    log_success "Pushed git tag to origin"
else
    log_error "Failed to push git tag"
    exit 1
fi

# Create GitHub release
log_info "Creating GitHub release..."

# Extract release notes from CHANGELOG.md for this version
RELEASE_NOTES=$(awk "/^## \[${MANAGER_VERSION}\]/{flag=1; next} /^## \[/{flag=0} flag" CHANGELOG.md)

if [ -z "$RELEASE_NOTES" ]; then
    log_warn "Could not extract release notes from CHANGELOG.md"
    RELEASE_NOTES="Release ${MANAGER_VERSION} with TEI ${TEI_VERSION}"
fi

# Create release body
RELEASE_BODY="$(cat <<EOF
${RELEASE_NOTES}

## Docker Images

\`\`\`bash
# Multi-arch (default)
docker pull ghcr.io/nazq/tei-manager:${MANAGER_VERSION}-tei-${TEI_VERSION}

# Ada Lovelace (RTX 4090/4080)
docker pull ghcr.io/nazq/tei-manager:${MANAGER_VERSION}-tei-${TEI_VERSION}-ada

# Hopper (H100/H200)
docker pull ghcr.io/nazq/tei-manager:${MANAGER_VERSION}-tei-${TEI_VERSION}-hopper
\`\`\`
EOF
)"

if gh release create "$GIT_TAG" \
    --title "v${MANAGER_VERSION}" \
    --notes "$RELEASE_BODY"; then
    log_success "Created GitHub release"
else
    log_error "Failed to create GitHub release"
    exit 1
fi

# Summary
echo ""
echo "========================================"
log_success "Release ${MANAGER_VERSION} complete!"
echo "========================================"
echo ""
echo "Version Details:"
echo "  TEI Manager: ${MANAGER_VERSION}"
echo "  TEI:         ${TEI_VERSION}"
echo ""
echo "Published images:"
echo "  ghcr.io/nazq/tei-manager:${MANAGER_VERSION}-tei-${TEI_VERSION}        (multi-arch)"
echo "  ghcr.io/nazq/tei-manager:${MANAGER_VERSION}-tei-${TEI_VERSION}-ada    (RTX 4090/4080)"
echo "  ghcr.io/nazq/tei-manager:${MANAGER_VERSION}-tei-${TEI_VERSION}-hopper (H100/H200)"
echo ""
echo "GitHub release: https://github.com/nazq/tei-manager/releases/tag/${GIT_TAG}"
echo ""