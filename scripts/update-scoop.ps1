#Requires -Version 5.1
<##
.SYNOPSIS
    Update or validate the Scoop manifest for aria2-rust.

.EXAMPLE
    ./scripts/update-scoop.ps1 -Version v0.3.2

.EXAMPLE
    ./scripts/update-scoop.ps1 -Check
##>

[CmdletBinding()]
param(
    [string]$Version,
    [string]$Repository = "balovess/aria2_rust",
    [switch]$Check
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repoRoot "scoop/aria2-rust.json"

function Get-VersionNumber {
    param([string]$Value)

    $normalized = $Value.Trim()
    if ($normalized.StartsWith("v", [System.StringComparison]::OrdinalIgnoreCase)) {
        $normalized = $normalized.Substring(1)
    }
    if ($normalized -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
        throw "Invalid release version: $Value"
    }
    return $normalized
}

function Get-TextResponse {
    param([string]$Uri)

    $tempFile = [System.IO.Path]::GetTempFileName()
    try {
        Invoke-WebRequest -Uri $Uri -OutFile $tempFile -UseBasicParsing -Headers @{ "User-Agent" = "aria2-rust-scoop" }
        return [System.IO.File]::ReadAllText($tempFile, [System.Text.Encoding]::UTF8)
    } finally {
        Remove-Item -LiteralPath $tempFile -Force -ErrorAction SilentlyContinue
    }
}

function Get-ReleaseHash {
    param(
        [string]$Repo,
        [string]$Tag,
        [string]$ArtifactName
    )

    $releaseUri = "https://api.github.com/repos/$Repo/releases/tags/$Tag"
    $release = Invoke-RestMethod -Uri $releaseUri -Headers @{ "User-Agent" = "aria2-rust-scoop" }
    $hashAssetName = "$ArtifactName.sha256"
    $hashAsset = @($release.assets | Where-Object { $_.name -eq $hashAssetName })
    if ($hashAsset.Count -ne 1) {
        throw "Release $Tag does not contain exactly one $hashAssetName asset"
    }

    $hashText = Get-TextResponse -Uri $hashAsset[0].browser_download_url
    $match = [regex]::Match($hashText, '(?i)\b[0-9a-f]{64}\b')
    if (-not $match.Success) {
        throw "Unable to find a SHA-256 hash in $hashAssetName"
    }
    return $match.Value.ToLowerInvariant()
}

function Assert-ScoopManifest {
    param([pscustomobject]$Manifest)

    $binary = $Manifest.architecture.'64bit'
    if ($Manifest.version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
        throw "Manifest version is invalid: $($Manifest.version)"
    }
    if ($binary.url -notmatch "/v$([regex]::Escape($Manifest.version))/aria2-x86_64-windows\.zip$") {
        throw "Manifest URL does not match version $($Manifest.version)"
    }
    if ($binary.hash -notmatch '^[0-9a-f]{64}$') {
        throw "Manifest contains an invalid SHA-256 hash"
    }
    if ($Manifest.checkver.github -ne "https://api.github.com/repos/balovess/aria2_rust/releases/latest") {
        throw "Manifest checkver must use the latest GitHub release API"
    }
    if ($Manifest.autoupdate.hash.url -ne '$url.sha256' -or
        $Manifest.autoupdate.hash.regex -ne '(?i)$sha256') {
        throw "Manifest must define the release SHA-256 autoupdate rule"
    }
    if (-not (@($Manifest.bin) | Where-Object { $_[0] -eq "aria2c.exe" -and $_[1] -eq "aria2c" })) {
        throw "Manifest must expose aria2c.exe as aria2c"
    }
}

function Test-RemoteArtifact {
    param([pscustomobject]$Manifest)

    $tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("aria2-scoop-" + [guid]::NewGuid().ToString("N"))
    $archivePath = Join-Path $tempRoot "aria2-x86_64-windows.zip"
    try {
        New-Item -ItemType Directory -Path $tempRoot | Out-Null
        Invoke-WebRequest -Uri $Manifest.architecture.'64bit'.url -OutFile $archivePath -UseBasicParsing
        $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $Manifest.architecture.'64bit'.hash) {
            throw "Remote artifact hash mismatch: expected $($Manifest.architecture.'64bit'.hash), got $actualHash"
        }
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $archive = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
        try {
            $entries = @($archive.Entries | ForEach-Object { $_.FullName })
        } finally {
            $archive.Dispose()
        }
        if (-not ($entries -contains "aria2c.exe")) {
            throw "Remote artifact does not contain aria2c.exe at the archive root"
        }
    } finally {
        if (Test-Path -LiteralPath $tempRoot) {
            Remove-Item -LiteralPath $tempRoot -Recurse -Force
        }
    }
}

if (-not (Test-Path -LiteralPath $manifestPath)) {
    throw "Scoop manifest not found: $manifestPath"
}

$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($Check) {
    Assert-ScoopManifest -Manifest $manifest
    Test-RemoteArtifact -Manifest $manifest
    Write-Host "Scoop manifest is valid and matches its remote Windows artifact."
    exit 0
}

if ([string]::IsNullOrWhiteSpace($Version)) {
    throw "-Version is required when updating the manifest"
}

$versionNumber = Get-VersionNumber -Value $Version
$tag = "v$versionNumber"
$artifactName = "aria2-x86_64-windows.zip"
$hash = Get-ReleaseHash -Repo $Repository -Tag $tag -ArtifactName $artifactName
$manifest.version = $versionNumber
$manifest.architecture.'64bit'.url = "https://github.com/$Repository/releases/download/$tag/$artifactName"
    $manifest.architecture.'64bit'.hash = $hash
Assert-ScoopManifest -Manifest $manifest

$json = $manifest | ConvertTo-Json -Depth 10
[System.IO.File]::WriteAllText($manifestPath, "$json`r`n", [System.Text.UTF8Encoding]::new($false))
Write-Host "Updated $manifestPath to $versionNumber ($hash)."
