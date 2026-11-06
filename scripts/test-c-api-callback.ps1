param(
    [string]$TargetDir = "target/c-api-callback-smoke"
)

$ErrorActionPreference = "Stop"

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw "This smoke script currently targets the Windows/MSVC cdylib import-library flow."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if ([IO.Path]::IsPathRooted($TargetDir)) {
    $buildRoot = [IO.Path]::GetFullPath($TargetDir)
} else {
    $buildRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot $TargetDir))
}
$debugDir = Join-Path $buildRoot "debug"
$source = Join-Path $repoRoot "aria2-core/tests/c_api_callback_smoke.c"
$includeDir = Join-Path $repoRoot "aria2-core/include"
$forwardIncludeDir = Join-Path $repoRoot "bindings/c/include"
$importLibrary = Join-Path $debugDir "aria2_core.dll.lib"
$consumer = Join-Path $debugDir "c_api_callback_smoke.exe"
$clang = Get-Command clang.exe -ErrorAction Stop

Push-Location $repoRoot
try {
    & cargo build --target-dir $buildRoot -p aria2-core -j 1
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    if (-not (Test-Path -LiteralPath $importLibrary)) {
        throw "Expected cdylib import library was not produced: $importLibrary"
    }

    & $clang.Source -std=c11 -Wall -Wextra -Werror -fsyntax-only -I $forwardIncludeDir $source
    if ($LASTEXITCODE -ne 0) {
        throw "C11 forwarding-header check failed with exit code $LASTEXITCODE"
    }

    & $clang.Source -std=c11 -Wall -Wextra -Werror -I $includeDir $source $importLibrary -o $consumer
    if ($LASTEXITCODE -ne 0) {
        throw "C11 consumer compilation/link failed with exit code $LASTEXITCODE"
    }

    Push-Location $debugDir
    try {
        & $consumer
        if ($LASTEXITCODE -ne 0) {
            throw "C11 consumer smoke failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
} finally {
    Pop-Location
}
