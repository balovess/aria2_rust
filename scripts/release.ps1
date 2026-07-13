#!/usr/bin/env pwsh
# release.ps1 - Orchestrate the release process
#
# Usage: ./scripts/release.ps1 -Level <LEVEL>
# LEVEL: major | minor | patch
#
# This script will:
# 1. Run tests
# 2. Bump version with cargo-release
# 3. Update CHANGELOG
# 4. Commit and tag
# 5. Push to trigger GitHub Actions

param(
    [Parameter(Mandatory=$true)]
    [ValidateSet("major", "minor", "patch")]
    [string]$Level
)

$ErrorActionPreference = "Stop"
$SCRIPT_DIR = Split-Path -Parent $MyInvocation.MyCommand.Path
$PROJECT_ROOT = Split-Path -Parent $SCRIPT_DIR

Write-Host "=== Release Process ==="
Write-Host "Level: $Level"
Write-Host ""

# Step 1: Run tests
Write-Host "Step 1: Running tests..."
Push-Location $PROJECT_ROOT
cargo test --workspace --all-targets
Write-Host "  ✓ Tests passed"
Write-Host ""

# Step 2: Bump version with cargo-release
Write-Host "Step 2: Bumping version..."
cargo release $Level --no-confirm --execute
Write-Host "  ✓ Version bumped"
Write-Host ""

# Step 3: Update CHANGELOG
Write-Host "Step 3: Please update CHANGELOG.md with the changes for this release."
Read-Host "Press Enter when done"
Write-Host "  ✓ CHANGELOG updated"
Write-Host ""

# Step 4: Commit changes
Write-Host "Step 4: Committing changes..."
$cargoContent = Get-Content "$PROJECT_ROOT/Cargo.toml" -Raw
if ($cargoContent -match 'version\s*=\s*"([^"]+)"') {
    $Version = $matches[1]
}
git add -A
git commit -m "chore: release v$Version"
git tag "v$Version"
Write-Host "  ✓ Committed and tagged"
Write-Host ""

# Step 5: Push to trigger GitHub Actions
Write-Host "Step 5: Pushing to remote..."
git push origin main
git push origin "v$Version"
Write-Host "  ✓ Pushed"
Write-Host ""

Pop-Location

Write-Host "=== Release Complete ==="
Write-Host "Version: $Version"
Write-Host "Tag: v$Version"
Write-Host ""
Write-Host "GitHub Actions will now:"
Write-Host "  - Build binaries for all platforms"
Write-Host "  - Create GitHub Release"
Write-Host "  - Publish to crates.io"
Write-Host "  - Publish Python SDK to PyPI"
Write-Host "  - Publish Node.js SDK to NPM"
Write-Host "  - Push Docker image"
