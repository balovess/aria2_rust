#Requires -Version 5.1
[CmdletBinding()]
param([string]$Version, [string]$Repository = "balovess/aria2_rust", [switch]$Check)
$ErrorActionPreference = "Stop"
$path = Join-Path (Split-Path -Parent $PSScriptRoot) "homebrew/aria2-rust.rb"
$formula = [IO.File]::ReadAllText($path)
if ($Check) {
    if ($formula -match 'PLACEHOLDER_SHA256') { throw "Formula contains placeholder hashes" }
    Write-Host "Homebrew formula is valid."
    exit 0
}
if (-not $Version) { throw "-Version is required" }
$tag = $Version.TrimStart('v')
if ($tag -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw "Invalid release version: $Version" }
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/tags/v$tag" -Headers @{ "User-Agent" = "aria2-rust-homebrew" }
$formula = $formula -replace 'version "[^"]+"', "version `"$tag`""
foreach ($artifact in @("aria2-x86_64-macos.tar.gz", "aria2-aarch64-macos.tar.gz", "aria2-x86_64-linux.tar.gz", "aria2-aarch64-linux.tar.gz")) {
    $asset = @($release.assets | Where-Object { $_.name -eq "$artifact.sha256" })
    if ($asset.Count -ne 1) { throw "Missing checksum asset: $artifact.sha256" }
    $text = (Invoke-WebRequest -Uri $asset[0].browser_download_url -UseBasicParsing).Content
    $match = [regex]::Match($text, '(?i)\b[0-9a-f]{64}\b')
    if (-not $match.Success) { throw "Invalid checksum asset: $artifact.sha256" }
    $pattern = '(?s)(url "[^"]*/' + [regex]::Escape($artifact) + '"\s*\r?\n\s*sha256 ")[^"]+'
    $formula = [regex]::Replace($formula, $pattern, ('$1' + $match.Value.ToLowerInvariant()), 1)
}
if ($formula -match 'PLACEHOLDER_SHA256') { throw "Not all hashes were updated" }
[IO.File]::WriteAllText($path, $formula + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
Write-Host "Updated Homebrew formula to $tag."
