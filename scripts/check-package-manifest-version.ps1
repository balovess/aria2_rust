#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Repository = $env:GITHUB_REPOSITORY,
    [string]$LatestVersion
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

function Get-LatestStableReleaseVersion {
    param([string]$Repo)

    $headers = @{
        Accept = "application/vnd.github+json"
        "User-Agent" = "aria2-rust-package-manifest-check"
    }
    if (-not [string]::IsNullOrWhiteSpace($env:GH_TOKEN)) {
        $headers.Authorization = "Bearer $env:GH_TOKEN"
    }

    try {
        $release = Invoke-RestMethod `
            -Uri "https://api.github.com/repos/$Repo/releases/latest" `
            -Headers $headers
    } catch {
        throw "Unable to query the latest GitHub release for $Repo`: $_"
    }

    if ($release.draft -or $release.prerelease -or
        $release.tag_name -notmatch '^v(?<version>[0-9]+\.[0-9]+\.[0-9]+)$') {
        throw "Latest release is not a stable vX.Y.Z release: $($release.tag_name)"
    }
    return $Matches.version
}

if ($Repository -notmatch '^[^/]+/[^/]+$') {
    throw "Invalid GitHub repository: $Repository"
}

if ([string]::IsNullOrWhiteSpace($LatestVersion)) {
    $LatestVersion = Get-LatestStableReleaseVersion -Repo $Repository
} elseif ($LatestVersion -notmatch '^v?(?<version>[0-9]+\.[0-9]+\.[0-9]+)$') {
    throw "Invalid stable release version: $LatestVersion"
} else {
    $LatestVersion = $Matches.version
}

$manifestPath = Join-Path $repoRoot "bucket/aria2-rust.json"
try {
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
} catch {
    throw "Unable to read Scoop manifest $manifestPath`: $_"
}

if ($manifest.version -ne $LatestVersion) {
    throw "Scoop manifest version is '$($manifest.version)'; latest release is $LatestVersion."
}

$expectedScoopUrl = "https://github.com/$Repository/releases/download/v$LatestVersion/aria2-x86_64-windows-full.zip"
if ($manifest.architecture.'64bit'.url -ne $expectedScoopUrl) {
    throw "Scoop URL does not target the latest release: $($manifest.architecture.'64bit'.url)"
}

$formulaPath = Join-Path $repoRoot "Formula/aria2-rust.rb"
$formula = Get-Content -LiteralPath $formulaPath -Raw
$formulaMatch = [regex]::Match(
    $formula,
    '(?m)^\s*url "https://github\.com/([^/]+)/([^/]+)/archive/refs/tags/(v[0-9.]+)\.tar\.gz"\s*$'
)
if (-not $formulaMatch.Success) {
    throw "Homebrew formula must use a versioned GitHub source archive URL."
}

$formulaRepository = "$($formulaMatch.Groups[1].Value)/$($formulaMatch.Groups[2].Value)"
$formulaTag = $formulaMatch.Groups[3].Value
$expectedTag = "v$LatestVersion"
if ($formulaRepository -ne $Repository -or $formulaTag -ne $expectedTag) {
    throw "Homebrew formula targets $formulaRepository@$formulaTag; expected $Repository@$expectedTag."
}

Write-Host "Scoop and Homebrew manifests both target the latest stable release $expectedTag ($Repository)."
