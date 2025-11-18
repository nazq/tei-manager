#!/usr/bin/env bash
# Release script for TEI Manager
# Builds and publishes Docker images with version and latest tags

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
DOCKER_REGISTRY="${DOCKER_REGISTRY:-docker.io}"
DOCKER_REPO="${DOCKER_REPO:-nazq/tei-manager}"
IMAGE_NAME="${DOCKER_REGISTRY}/${DOCKER_REPO}"

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

# Check if version is provided
if [ $# -eq 0 ]; then
    log_error "No version specified"
    echo "Usage: $0 <version>"
    echo "Example: $0 1.8.3"
    exit 1
fi

VERSION=$1

# Validate version format (semantic versioning)
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    log_error "Invalid version format: $VERSION"
    echo "Version must follow semantic versioning: MAJOR.MINOR.PATCH"
    echo "Example: 1.8.3"
    exit 1
fi

log_info "Starting release process for version $VERSION"
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
if [ "$CARGO_VERSION" != "$VERSION" ]; then
    log_warn "Cargo.toml version ($CARGO_VERSION) doesn't match release version ($VERSION)"
    read -p "Update Cargo.toml version to $VERSION? (y/N) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        sed -i "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml
        log_success "Updated Cargo.toml version to $VERSION"
    else
        log_error "Version mismatch - please update Cargo.toml manually"
        exit 1
    fi
fi

# Run tests
log_info "Running tests..."
if cargo test --quiet; then
    log_success "Unit tests passed"
else
    log_error "Unit tests failed"
    exit 1
fi

# Run clippy
log_info "Running clippy..."
if cargo clippy --quiet -- -D warnings; then
    log_success "Clippy checks passed"
else
    log_error "Clippy checks failed"
    exit 1
fi

# Run end-to-end tests
log_info "Running end-to-end tests..."
if ./test-e2e.sh >/dev/null 2>&1; then
    log_success "E2E tests passed"
else
    log_error "E2E tests failed - run './test-e2e.sh' for details"
    exit 1
fi

# Build Docker image
log_info "Building Docker image..."
log_info "Image: ${IMAGE_NAME}:${VERSION}"

if docker build -t "${IMAGE_NAME}:${VERSION}" .; then
    log_success "Docker image built successfully"
else
    log_error "Docker build failed"
    exit 1
fi

# Tag as latest
log_info "Tagging as latest..."
docker tag "${IMAGE_NAME}:${VERSION}" "${IMAGE_NAME}:latest"
log_success "Tagged as ${IMAGE_NAME}:latest"

# Show image details
echo ""
log_info "Image details:"
docker images "${IMAGE_NAME}" --filter "reference=${IMAGE_NAME}:${VERSION}"
docker images "${IMAGE_NAME}" --filter "reference=${IMAGE_NAME}:latest"
echo ""

# Ask for confirmation to push
log_warn "Ready to push images to registry"
echo "  - ${IMAGE_NAME}:${VERSION}"
echo "  - ${IMAGE_NAME}:latest"
echo ""
read -p "Push to registry? (y/N) " -n 1 -r
echo

if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    log_info "Skipping push - images built locally only"
    log_success "Release build complete (local only)"
    exit 0
fi

# Login to Docker registry (if not already logged in)
log_info "Checking Docker registry login..."
if docker info 2>/dev/null | grep -q "Username:"; then
    log_success "Already logged in to Docker registry"
else
    log_info "Please log in to Docker registry"
    if ! docker login "${DOCKER_REGISTRY}"; then
        log_error "Docker login failed"
        exit 1
    fi
fi

# Push version tag
log_info "Pushing ${IMAGE_NAME}:${VERSION}..."
if docker push "${IMAGE_NAME}:${VERSION}"; then
    log_success "Pushed ${IMAGE_NAME}:${VERSION}"
else
    log_error "Failed to push version tag"
    exit 1
fi

# Push latest tag
log_info "Pushing ${IMAGE_NAME}:latest..."
if docker push "${IMAGE_NAME}:latest"; then
    log_success "Pushed ${IMAGE_NAME}:latest"
else
    log_error "Failed to push latest tag"
    exit 1
fi

# Create git tag
log_info "Creating git tag v${VERSION}..."
if git tag -a -f "v${VERSION}" -m "Release version ${VERSION}"; then
    log_success "Created git tag v${VERSION}"

    read -p "Push git tag to origin? (y/N) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        if git push -f origin "v${VERSION}"; then
            log_success "Pushed git tag to origin"
        else
            log_warn "Failed to push git tag (you may need to push manually)"
        fi
    fi
else
    log_warn "Git tag creation failed"
fi

# Summary
echo ""
echo "========================================"
log_success "Release ${VERSION} complete!"
echo "========================================"
echo ""
echo "Published images:"
echo "  - ${IMAGE_NAME}:${VERSION}"
echo "  - ${IMAGE_NAME}:latest"
echo ""
echo "Next steps:"
echo "  1. Create GitHub release at https://github.com/nazq/tei-manager/releases/new"
echo "  2. Tag: v${VERSION}"
echo "  3. Add release notes from CHANGELOG.md"
echo "  4. Attach any binaries if needed"
echo ""
log_info "Pull the image with:"
echo "  docker pull ${IMAGE_NAME}:${VERSION}"
echo "  docker pull ${IMAGE_NAME}:latest"
echo ""
