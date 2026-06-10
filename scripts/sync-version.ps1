#!/usr/bin/env pwsh
# sync-version.ps1 - Synchronize version from Cargo.toml to all other files
#
# Usage: ./scripts/sync-version.ps1 [-Version <VERSION>]
# If VERSION is not provided, it will be read from Cargo.toml

param(
    [string]$Version
)

$ErrorActionPreference = "Stop"
$SCRIPT_DIR = Split-Path -Parent $MyInvocation.MyCommand.Path
$PROJECT_ROOT = Split-Path -Parent $SCRIPT_DIR

# Get version from argument or Cargo.toml
if ([string]::IsNullOrEmpty($Version)) {
    $cargoContent = Get-Content "$PROJECT_ROOT/Cargo.toml" -Raw
    if ($cargoContent -match 'version\s*=\s*"([^"]+)"') {
        $Version = $matches[1]
    }
}

if ([string]::IsNullOrEmpty($Version)) {
    Write-Error "Could not determine version"
    exit 1
}

Write-Host "Syncing version $Version to all files..."

# 1. Update Homebrew formula
$homebrewFile = "$PROJECT_ROOT/homebrew/aria2-rust.rb"
if (Test-Path $homebrewFile) {
    $content = Get-Content $homebrewFile -Raw
    $content = $content -replace 'version "[^"]*"', "version `"$Version`""
    Set-Content $homebrewFile $content -NoNewline
    Write-Host "  ✓ Updated homebrew/aria2-rust.rb"
}

# 2. Update Scoop manifest
$scoopFile = "$PROJECT_ROOT/scoop/aria2-rust.json"
if (Test-Path $scoopFile) {
    $content = Get-Content $scoopFile -Raw
    $content = $content -replace '"version": "[^"]*"', "`"version`": `"$Version`""
    $content = $content -replace 'releases/download/v[^/]*', "releases/download/v$Version"
    Set-Content $scoopFile $content -NoNewline
    Write-Host "  ✓ Updated scoop/aria2-rust.json"
}

# 3. Update Node.js SDK package.json
$nodejsFile = "$PROJECT_ROOT/bindings/nodejs/package.json"
if (Test-Path $nodejsFile) {
    $content = Get-Content $nodejsFile -Raw
    $content = $content -replace '"version": "[^"]*"', "`"version`": `"$Version`""
    Set-Content $nodejsFile $content -NoNewline
    Write-Host "  ✓ Updated bindings/nodejs/package.json"
}

# 4. Update Python SDK pyproject.toml
$pythonFile = "$PROJECT_ROOT/bindings/python/pyproject.toml"
if (Test-Path $pythonFile) {
    $content = Get-Content $pythonFile -Raw
    $content = $content -replace '^version = "[^"]*"', "version = `"$Version`""
    Set-Content $pythonFile $content -NoNewline
    Write-Host "  ✓ Updated bindings/python/pyproject.toml"
}

Write-Host ""
Write-Host "All files synchronized to version $Version"
