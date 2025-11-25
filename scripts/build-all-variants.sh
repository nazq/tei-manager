#!/usr/bin/env bash
# ============================================================================
# Build all public TEI-manager variants for a release
# ============================================================================
#
# Builds public images based on ./Dockerfile for general distribution.
# For private images with mTLS certs, see ./private/build.sh
#
# Uses BuildKit for:
#   - Cargo registry cache (shared across builds)
#   - Build artifact cache (incremental compilation)
#
# Usage:
#   ./scripts/build-all-variants.sh 0.3.0 1.8.3         # Build public images only
#   ./scripts/build-all-variants.sh 0.3.0 1.8.3 --push  # Build + push public images
#
# ============================================================================

set -euo pipefail

# Enable BuildKit for cache mounts
export DOCKER_BUILDKIT=1

MANAGER_VERSION="${1:?Missing tei-manager version (e.g., 0.3.0)}"
TEI_VERSION="${2:?Missing TEI version (e.g., 1.8.3)}"
PUSH="${3:-}"

REPO="nazq/tei-manager"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo "======================================"
echo "Building TEI Manager Public Variants"
echo "======================================"
echo "tei-manager version: $MANAGER_VERSION"
echo "TEI version:         $TEI_VERSION"
echo "Repository:          $REPO"
echo "Push:                ${PUSH:-no}"
echo "======================================"
echo

# ============================================================================
# Build function for public images (parameterized Dockerfile)
# ============================================================================
build_public_variant() {
    local variant_name="$1"
    local tei_variant="$2"
    local tei_ver="$3"
    local variant_suffix="$4"
    local variant_label="$5"
    local variant_desc="$6"
    shift 6
    local tags=("$@")

    echo -e "${BLUE}[BUILD]${NC} Building $variant_name variant..."
    echo "  Dockerfile:    Dockerfile"
    echo "  TEI_VARIANT:   ${tei_variant:-<default>}"
    echo "  TEI_VERSION:   $tei_ver"
    echo "  Tags:"
    for tag in "${tags[@]}"; do
        echo "    - $REPO:$tag"
    done

    # Build with all tags
    local tag_args=()
    for tag in "${tags[@]}"; do
        tag_args+=("-t" "$REPO:$tag")
    done

    docker build \
        -f Dockerfile \
        --build-arg TEI_VARIANT="$tei_variant" \
        --build-arg TEI_VERSION="$tei_ver" \
        --build-arg VARIANT_SUFFIX="$variant_suffix" \
        --build-arg VARIANT_NAME="$variant_label" \
        --build-arg VARIANT_DESC="$variant_desc" \
        "${tag_args[@]}" \
        . || {
            echo -e "${RED}[FAIL]${NC} Build failed for $variant_name"
            exit 1
        }

    echo -e "${GREEN}[OK]${NC} $variant_name variant built successfully"
    echo

    # Push if requested
    if [ "$PUSH" = "--push" ]; then
        for tag in "${tags[@]}"; do
            echo -e "${BLUE}[PUSH]${NC} Pushing $REPO:$tag"
            docker push "$REPO:$tag" || {
                echo -e "${RED}[FAIL]${NC} Push failed for $tag"
                exit 1
            }
        done
        echo -e "${GREEN}[OK]${NC} All tags pushed for $variant_name"
        echo
    fi
}

# ============================================================================
# Build all public variants
# ============================================================================

# Multi-arch variant (default - works everywhere)
build_public_variant \
    "multi-arch (default)" \
    "" \
    "$TEI_VERSION" \
    "" \
    "" \
    "" \
    "${MANAGER_VERSION}-tei-${TEI_VERSION}"

# Ada Lovelace variant (RTX 4090/4080 optimized)
build_public_variant \
    "ada (RTX 4090/4080)" \
    "89-" \
    "$TEI_VERSION" \
    "ada" \
    "Ada Lovelace" \
    " - Optimized for RTX 4090/4080" \
    "${MANAGER_VERSION}-tei-${TEI_VERSION}-ada"

# Hopper variant (H100/H200 optimized)
build_public_variant \
    "hopper (H100/H200)" \
    "hopper-" \
    "$TEI_VERSION" \
    "hopper" \
    "Hopper" \
    " for H100/H200 GPUs" \
    "${MANAGER_VERSION}-tei-${TEI_VERSION}-hopper"

# ============================================================================
# Summary
# ============================================================================
echo "======================================"
echo -e "${GREEN}Public images built successfully!${NC}"
echo "======================================"
echo
echo "Images created:"
echo "  Multi-arch: $REPO:${MANAGER_VERSION}-tei-${TEI_VERSION}"
echo "  Ada:        $REPO:${MANAGER_VERSION}-tei-${TEI_VERSION}-ada"
echo "  Hopper:     $REPO:${MANAGER_VERSION}-tei-${TEI_VERSION}-hopper"
echo

if [ "$PUSH" != "--push" ]; then
    echo -e "${YELLOW}Note:${NC} Images built locally only (not pushed)"
    echo "To push to Docker Hub, run:"
    echo "  $0 $MANAGER_VERSION $TEI_VERSION --push"
    echo
fi

exit 0
