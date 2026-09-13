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
# 4. Commit
# 5. Push the current branch and open a PR into master

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

# Step 2: Bump versions with cargo-release (package versions are independent)
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
$cargoContent = Get-Content "$PROJECT_ROOT/aria2/Cargo.toml" -Raw
if ($cargoContent -match 'version\s*=\s*"([^"]+)"') {
    $Version = $matches[1]
}
git add -A
git commit -m "chore: release v$Version"
Write-Host "  ✓ Committed"
Write-Host ""

# Step 5: Push to trigger GitHub Actions
Write-Host "Step 5: Pushing the current branch..."
$Branch = (git branch --show-current).Trim()
if ([string]::IsNullOrWhiteSpace($Branch) -or $Branch -eq "master") {
    throw "Release script must run on a non-master development branch; open a PR into master."
}
git push origin $Branch
Write-Host "  ✓ Pushed $Branch"
Write-Host ""

Pop-Location

Write-Host "=== Release Complete ==="
Write-Host "Version: $Version"
Write-Host "Tag: v$Version"
Write-Host ""
Write-Host "Open a pull request from $Branch into master. After it is merged, GitHub Actions will:"
Write-Host "  - Build binaries for all platforms"
Write-Host "  - Create GitHub Release"
Write-Host "  - Publish to crates.io"
Write-Host "  - Publish Python SDK to PyPI"
Write-Host "  - Publish Node.js SDK to NPM"
Write-Host "  - Push Docker image"
