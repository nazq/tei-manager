#!/usr/bin/env bash
#
# Updates version references in README.md from .release-please-manifest.json
#
# Usage: ./scripts/update-readme-version.sh [--commit]
#
# Options:
#   --commit    Stage and commit the changes (no push)
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MANIFEST="$REPO_ROOT/.release-please-manifest.json"
README="$REPO_ROOT/README.md"

# Parse args
COMMIT=false
if [[ "${1:-}" == "--commit" ]]; then
    COMMIT=true
fi

# Extract version from manifest
if [[ ! -f "$MANIFEST" ]]; then
    echo "ERROR: $MANIFEST not found"
    exit 1
fi

VERSION=$(jq -r '.["."]' "$MANIFEST")

if [[ -z "$VERSION" || "$VERSION" == "null" ]]; then
    echo "ERROR: Could not extract version from $MANIFEST"
    exit 1
fi

echo "Updating README.md to version $VERSION"

# Replace version pattern: X.Y.Z-tei- where X.Y.Z is the old version
# This handles tags like 0.8.0-tei-1.8.3, 0.8.0-tei-1.8.3-ada, etc.
sed -i -E "s/[0-9]+\.[0-9]+\.[0-9]+(-tei-)/${VERSION}\1/g" "$README"

# Show what changed
echo ""
echo "Changes made:"
git diff --stat "$README" 2>/dev/null || true
echo ""
git diff "$README" 2>/dev/null | head -40 || true

if [[ "$COMMIT" == "true" ]]; then
    echo ""
    echo "Committing changes..."
    git add "$README"
    git commit -m "chore: update README version references to $VERSION"
    echo ""
    echo "Committed. Run 'git push' when ready."
else
    echo ""
    echo "Run with --commit to stage and commit, or manually:"
    echo "  git add README.md"
    echo "  git commit -m \"chore: update README version references to $VERSION\""
fi
