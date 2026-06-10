#Requires -Version 5.1
#
# aria2-rust installer script for Windows (PowerShell)
#
# Usage:
#   irm https://raw.githubusercontent.com/balovess/aria2_rust/main/install.ps1 | iex
#   or
#   irm https://raw.githubusercontent.com/balovess/aria2_rust/main/install.ps1 | iex; Install-Aria2Rust -Version "v0.1.0"
#
# Options:
#   -Version VERSION    Install a specific version (default: latest)
#   -Prefix DIR         Installation directory (default: C:\Program Files\aria2-rust or ~/AppData/Local/Programs/aria2-rust)
#   -Help               Show this help message

param(
    [string]$Version = "latest",
    [string]$Prefix = "",
    [switch]$Help
)

# Configuration
$Repo = "balovess/aria2_rust"
$BinaryName = "aria2c.exe"
$DefaultVersion = "latest"

# Show help
if ($Help) {
    Write-Host "aria2-rust installer for Windows"
    Write-Host ""
    Write-Host "Usage: irm <url> | iex; Install-Aria2Rust [options]"
    Write-Host ""
    Write-Host "Options:"
    Write-Host "  -Version VERSION    Install a specific version (default: latest)"
    Write-Host "  -Prefix DIR         Installation directory (default: C:\Program Files\aria2-rust or ~/AppData/Local/Programs/aria2-rust)"
    Write-Host "  -Help               Show this help message"
    Write-Host ""
    Write-Host "Examples:"
    Write-Host "  # Install latest version"
    Write-Host '  irm https://raw.githubusercontent.com/balovess/aria2_rust/main/install.ps1 | iex'
    Write-Host ""
    Write-Host "  # Install specific version"
    Write-Host '  irm https://raw.githubusercontent.com/balovess/aria2_rust/main/install.ps1 | iex; Install-Aria2Rust -Version "v0.1.0"'
    Write-Host ""
    Write-Host "  # Install to custom directory"
    Write-Host '  irm https://raw.githubusercontent.com/balovess/aria2_rust/main/install.ps1 | iex; Install-Aria2Rust -Prefix "C:\Tools\aria2-rust"'
    exit 0
}

# Print functions
function Write-Info { param($Message) Write-Host "[INFO] $Message" -ForegroundColor Blue }
function Write-Success { param($Message) Write-Host "[SUCCESS] $Message" -ForegroundColor Green }
function Write-Warning { param($Message) Write-Host "[WARN] $Message" -ForegroundColor Yellow }
function Write-Error { param($Message) Write-Host "[ERROR] $Message" -ForegroundColor Red; exit 1 }

# Detect architecture
function Get-Architecture {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        "X64" { return "x86_64" }
        "Arm64" { return "aarch64" }
        default { Write-Error "Unsupported architecture: $arch" }
    }
}

# Determine installation directory
function Get-InstallPrefix {
    if ([string]::IsNullOrEmpty($Prefix)) {
        # Try Program Files first, fall back to AppData
        $programFiles = "${env:ProgramFiles}\aria2-rust"
        $appData = "${env:LOCALAPPDATA}\Programs\aria2-rust"
        
        # Check if we can write to Program Files (requires admin)
        try {
            $testFile = Join-Path $programFiles ".write_test"
            New-Item -ItemType Directory -Path $programFiles -Force -ErrorAction Stop | Out-Null
            Remove-Item $testFile -Force -ErrorAction SilentlyContinue
            return $programFiles
        } catch {
            return $appData
        }
    }
    return $Prefix
}

# Get latest version from GitHub API
function Get-LatestVersion {
    $apiUrl = "https://api.github.com/repos/$Repo/releases/latest"
    try {
        $response = Invoke-RestMethod -Uri $apiUrl -UseBasicParsing
        return $response.tag_name
    } catch {
        return ""
    }
}

# Add to PATH if not already there
function Add-ToPath {
    param($InstallDir)
    
    $userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($userPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("PATH", "$userPath;$InstallDir", "User")
        Write-Info "Added $InstallDir to user PATH"
    }
}

# Main installation function
function Install-Aria2Rust {
    # Banner
    Write-Host ""
    Write-Host "  ___  _ __ ___ _ __ ___   __ _ _ __" -ForegroundColor Cyan
    Write-Host " / _ \| '__/ _ \ '_ \` _ \ / _\` | '_ \" -ForegroundColor Cyan
    Write-Host "| (_) | | |  __/ | | | | | (_| | | | |" -ForegroundColor Cyan
    Write-Host " \___/|_|  \___|_| |_| |_|\__,_|_| |_|" -ForegroundColor Cyan
    Write-Host "        Rust Edition Installer" -ForegroundColor Cyan
    Write-Host ""

    # Detect platform
    $arch = Get-Architecture
    $artifact = "aria2-$arch-windows"
    $ext = "zip"
    Write-Info "Detected platform: Windows/$arch"

    # Determine install directory
    $installDir = Get-InstallPrefix
    Write-Info "Installation directory: $installDir"

    # Resolve version
    if ($Version -eq "latest") {
        Write-Info "Fetching latest version..."
        $Version = Get-LatestVersion
        if ([string]::IsNullOrEmpty($Version)) {
            Write-Warning "Could not determine latest version, using v0.1.0"
            $Version = "v0.1.0"
        }
    }

    Write-Info "Installing aria2-rust $Version"

    # Construct download URL
    $downloadUrl = "https://github.com/$Repo/releases/download/$Version/$artifact.$ext"

    # Create temp directory
    $tempDir = New-TemporaryDirectory
    $archiveFile = Join-Path $tempDir "$artifact.$ext"

    # Download
    Write-Info "Downloading from $downloadUrl..."
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile $archiveFile -UseBasicParsing
    } catch {
        Write-Error "Failed to download $downloadUrl"
    }

    # Extract
    Write-Info "Extracting..."
    Expand-Archive -Path $archiveFile -DestinationPath $tempDir -Force

    # Find binary
    $binaryPath = Join-Path $tempDir $BinaryName
    if (-not (Test-Path $binaryPath)) {
        # Binary might be in a subdirectory
        $binaryPath = Get-ChildItem -Path $tempDir -Filter $BinaryName -Recurse | Select-Object -First 1 -ExpandProperty FullName
    }

    if (-not (Test-Path $binaryPath)) {
        Write-Error "Binary not found in archive"
    }

    # Create install directory
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null

    # Install
    Write-Info "Installing to $installDir..."
    $destBinary = Join-Path $installDir $BinaryName
    Move-Item -Path $binaryPath -Destination $destBinary -Force

    # Cleanup
    Remove-Item -Path $tempDir -Recurse -Force

    # Add to PATH
    Add-ToPath $installDir

    # Verify installation
    Write-Success "aria2-rust installed successfully to $destBinary"

    # Show usage hint
    Write-Host ""
    Write-Host "Quick start:"
    Write-Host "  aria2c http://example.com/file.zip"
    Write-Host ""
    Write-Host "For more information, run: aria2c --help"
    Write-Host ""
    Write-Warning "You may need to restart your terminal for PATH changes to take effect"
}

# Helper function to create temporary directory
function New-TemporaryDirectory {
    $tempPath = [System.IO.Path]::GetTempPath()
    $tempDir = [System.IO.Path]::Combine($tempPath, [System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $tempDir | Out-Null
    return $tempDir
}

# Run installation
Install-Aria2Rust
