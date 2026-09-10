#Requires -Version 5.1
[CmdletBinding()]
param([string]$Version, [string]$Repository = "balovess/aria2_rust", [switch]$Check)
$ErrorActionPreference = "Stop"
$path = Join-Path (Split-Path -Parent $PSScriptRoot) "Formula/aria2-rust.rb"
$formula = [IO.File]::ReadAllText($path)
if ($Check) {
    if ($formula -notmatch 'url "https://github\.com/[^/"]+/[^/"]+/archive/refs/tags/v[0-9]+\.[0-9]+\.[0-9]+\.tar\.gz"') {
        throw "Formula must use a versioned GitHub source archive"
    }
    if ($formula -notmatch '(?m)^  sha256 "[0-9a-f]{64}"$') {
        throw "Formula must contain a valid source archive SHA-256"
    }
    if ($formula -notmatch '(?m)^  depends_on "rust" => :build$' -or
        $formula -notmatch '"--features", "full"') {
        throw "Formula must build the full feature set with Rust"
    }
    Write-Host "Homebrew formula is valid."
    exit 0
}
if (-not $Version) { throw "-Version is required" }
$tag = $Version.TrimStart('v')
if ($tag -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw "Invalid release version: $Version" }
$null = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/tags/v$tag" -Headers @{ "User-Agent" = "aria2-rust-homebrew" }
$sourceUrl = "https://github.com/$Repository/archive/refs/tags/v$tag.tar.gz"
$sourcePath = [IO.Path]::GetTempFileName()
try {
    Invoke-WebRequest -Uri $sourceUrl -OutFile $sourcePath -UseBasicParsing -Headers @{ "User-Agent" = "aria2-rust-homebrew" }
    $hash = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
} finally {
    Remove-Item -LiteralPath $sourcePath -Force -ErrorAction SilentlyContinue
}
$formula = $formula -replace 'url "[^"]+"', "url `"$sourceUrl`""
$formula = [regex]::Replace($formula, '(?m)(^  sha256 ")[^"]+', ('${1}' + $hash), 1)
$formula = $formula.TrimEnd([char[]]"`r`n")
[IO.File]::WriteAllText($path, $formula + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
Write-Host "Updated Homebrew formula to $tag."
