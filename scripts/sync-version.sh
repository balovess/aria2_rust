#!/usr/bin/env bash
# sync-version.sh - Synchronize version from Cargo.toml to all other files
#
# Usage: ./scripts/sync-version.sh [VERSION]
# If VERSION is not provided, it will be read from Cargo.toml

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Get version from argument or Cargo.toml
if [ -n "$1" ]; then
    VERSION="$1"
else
    VERSION=$(grep -oP 'version\s*=\s*"\K[^"]+' "$PROJECT_ROOT/Cargo.toml" | head -1)
fi

if [ -z "$VERSION" ]; then
    echo "Error: Could not determine version"
    exit 1
fi

echo "Syncing version $VERSION to all files..."

# 1. Update Homebrew formula
HOMEBREW_FILE="$PROJECT_ROOT/homebrew/aria2-rust.rb"
if [ -f "$HOMEBREW_FILE" ]; then
    sed -i.bak "s/version \".*\"/version \"$VERSION\"/" "$HOMEBREW_FILE"
    rm -f "$HOMEBREW_FILE.bak"
    echo "  ✓ Updated homebrew/aria2-rust.rb"
fi

# 2. Update Scoop manifest
SCOOP_FILE="$PROJECT_ROOT/scoop/aria2-rust.json"
if [ -f "$SCOOP_FILE" ]; then
    # Update version field
    sed -i.bak "s/\"version\": \".*\"/\"version\": \"$VERSION\"/" "$SCOOP_FILE"
    # Update URL in autoupdate section
    sed -i.bak "s|releases/download/v[^/]*|releases/download/v$VERSION|g" "$SCOOP_FILE"
    rm -f "$SCOOP_FILE.bak"
    echo "  ✓ Updated scoop/aria2-rust.json"
fi

# 3. Update Node.js SDK package.json
NODEJS_FILE="$PROJECT_ROOT/bindings/nodejs/package.json"
if [ -f "$NODEJS_FILE" ]; then
    sed -i.bak "s/\"version\": \".*\"/\"version\": \"$VERSION\"/" "$NODEJS_FILE"
    rm -f "$NODEJS_FILE.bak"
    echo "  ✓ Updated bindings/nodejs/package.json"
fi

# 4. Update Python SDK pyproject.toml
PYTHON_FILE="$PROJECT_ROOT/bindings/python/pyproject.toml"
if [ -f "$PYTHON_FILE" ]; then
    sed -i.bak "s/^version = \".*\"/version = \"$VERSION\"/" "$PYTHON_FILE"
    rm -f "$PYTHON_FILE.bak"
    echo "  ✓ Updated bindings/python/pyproject.toml"
fi

echo ""
echo "All files synchronized to version $VERSION"
