#Requires -Version 5.1
[CmdletBinding()]
param([string]$Version, [string]$Repository = 'balovess/aria2_rust', [switch]$Check)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$nuspecPath = Join-Path $root 'chocolatey/aria2-rust.nuspec'
$installPath = Join-Path $root 'chocolatey/tools/chocolateyInstall.ps1'
$artifact = 'aria2-x86_64-windows-full.zip'

function Get-Hash([string]$Repo, [string]$Tag) {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/tags/$Tag" -Headers @{ 'User-Agent' = 'aria2-rust-chocolatey' }
    $asset = @($release.assets | Where-Object { $_.name -eq "$artifact.sha256" })
    if ($asset.Count -ne 1) { throw "Release $Tag does not contain exactly one $artifact.sha256 asset" }
    $text = (Invoke-WebRequest -Uri $asset[0].browser_download_url -UseBasicParsing).Content
    $match = [regex]::Match($text, '(?i)\b[0-9a-f]{64}\b')
    if (-not $match.Success) { throw "Invalid checksum asset: $artifact.sha256" }
    $match.Value.ToLowerInvariant()
}

if (-not (Test-Path -LiteralPath $nuspecPath) -or -not (Test-Path -LiteralPath $installPath)) {
    throw 'Chocolatey package files are missing'
}

$nuspec = Get-Content -Raw -LiteralPath $nuspecPath
$install = Get-Content -Raw -LiteralPath $installPath
if ($Check) {
    if ($nuspec -match '<version>0\.0\.0</version>' -or $install -match 'PLACEHOLDER_SHA256') {
        throw 'Chocolatey package contains placeholder metadata'
    }
    if ($install -notmatch [regex]::Escape("aria2-x86_64-windows-full.zip")) {
        throw 'Chocolatey package does not use the full Windows artifact'
    }
    Write-Host 'Chocolatey package metadata is valid.'
    exit 0
}

if ([string]::IsNullOrWhiteSpace($Version)) { throw '-Version is required' }
$versionNumber = $Version.TrimStart('v')
if ($versionNumber -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw "Invalid release version: $Version" }
$hash = Get-Hash $Repository "v$versionNumber"
$nuspec = $nuspec -replace '<version>[^<]+</version>', "<version>$versionNumber</version>"
$install = $install -replace "releases/download/v[^/]+/$artifact", "releases/download/v$versionNumber/$artifact"
$install = $install -replace "\$checksum = '[^']+'", "`$checksum = '$hash'"
[IO.File]::WriteAllText($nuspecPath, $nuspec, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText($installPath, $install, [Text.UTF8Encoding]::new($false))
Write-Host "Updated Chocolatey package to $versionNumber."
