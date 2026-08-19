#!/usr/bin/env bash
# release.sh - Orchestrate the release process
#
# Usage: ./scripts/release.sh <LEVEL>
# LEVEL: major | minor | patch
#
# This script will:
# 1. Run tests
# 2. Bump version with cargo-release
# 3. Update CHANGELOG
# 4. Commit and tag
# 5. Push to trigger GitHub Actions

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Validate argument
LEVEL="${1:-patch}"
if [[ ! "$LEVEL" =~ ^(major|minor|patch)$ ]]; then
    echo "Usage: $0 <major|minor|patch>"
    exit 1
fi

echo "=== Release Process ==="
echo "Level: $LEVEL"
echo ""

# Step 1: Run tests
echo "Step 1: Running tests..."
cd "$PROJECT_ROOT"
cargo test --workspace --all-targets
echo "  ✓ Tests passed"
echo ""

# Step 2: Bump versions with cargo-release (package versions are independent)
echo "Step 2: Bumping version..."
cargo release "$LEVEL" --no-confirm --execute
echo "  ✓ Version bumped"
echo ""

# Step 3: Update CHANGELOG
echo "Step 3: Please update CHANGELOG.md with the changes for this release."
echo "Press Enter when done..."
read -r
echo "  ✓ CHANGELOG updated"
echo ""

# Step 4: Commit changes
echo "Step 4: Committing changes..."
VERSION=$(grep -oP 'version\s*=\s*"\K[^"]+' "$PROJECT_ROOT/aria2/Cargo.toml" | head -1)
git add -A
git commit -m "chore: release v$VERSION"
git tag "v$VERSION"
echo "  ✓ Committed and tagged"
echo ""

# Step 5: Push to trigger GitHub Actions
echo "Step 5: Pushing to remote..."
git push origin main
git push origin "v$VERSION"
echo "  ✓ Pushed"
echo ""

echo "=== Release Complete ==="
echo "Version: $VERSION"
echo "Tag: v$VERSION"
echo ""
echo "GitHub Actions will now:"
echo "  - Build binaries for all platforms"
echo "  - Create GitHub Release"
echo "  - Publish to crates.io"
echo "  - Publish Python SDK to PyPI"
echo "  - Publish Node.js SDK to NPM"
echo "  - Push Docker image"
