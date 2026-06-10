#!/bin/bash
#
# aria2-rust installer script for Linux and macOS
# 
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/balovess/aria2_rust/main/install.sh | bash
#   or
#   curl -fsSL https://raw.githubusercontent.com/balovess/aria2_rust/main/install.sh | bash -s -- --version v0.1.0
#
# Options:
#   --version VERSION    Install a specific version (default: latest)
#   --prefix DIR         Installation directory (default: /usr/local/bin or ~/.local/bin)
#   --help               Show this help message

set -e

# Configuration
REPO="balovess/aria2_rust"
BINARY_NAME="aria2c"
DEFAULT_VERSION="latest"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Print functions
info() { echo -e "${BLUE}[INFO]${NC} $1"; }
success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

# Show help
show_help() {
    echo "aria2-rust installer for Linux and macOS"
    echo ""
    echo "Usage: curl -fsSL <url> | bash [options]"
    echo ""
    echo "Options:"
    echo "  --version VERSION    Install a specific version (default: latest)"
    echo "  --prefix DIR         Installation directory (default: /usr/local/bin or ~/.local/bin)"
    echo "  --help               Show this help message"
    echo ""
    echo "Examples:"
    echo "  # Install latest version"
    echo "  curl -fsSL https://raw.githubusercontent.com/balovess/aria2_rust/main/install.sh | bash"
    echo ""
    echo "  # Install specific version"
    echo "  curl -fsSL https://raw.githubusercontent.com/balovess/aria2_rust/main/install.sh | bash -s -- --version v0.1.0"
    echo ""
    echo "  # Install to custom directory"
    echo "  curl -fsSL https://raw.githubusercontent.com/balovess/aria2_rust/main/install.sh | bash -s -- --prefix ~/.local/bin"
    exit 0
}

# Parse arguments
VERSION="$DEFAULT_VERSION"
PREFIX=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --version)
            VERSION="$2"
            shift 2
            ;;
        --prefix)
            PREFIX="$2"
            shift 2
            ;;
        --help)
            show_help
            ;;
        *)
            error "Unknown option: $1"
            ;;
    esac
done

# Detect OS and architecture
detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"
    
    case "$OS" in
        Linux)
            OS="linux"
            ;;
        Darwin)
            OS="macos"
            ;;
        *)
            error "Unsupported OS: $OS"
            ;;
    esac
    
    case "$ARCH" in
        x86_64|amd64)
            ARCH="x86_64"
            ;;
        aarch64|arm64)
            ARCH="aarch64"
            ;;
        *)
            error "Unsupported architecture: $ARCH"
            ;;
    esac
    
    # Set artifact name
    if [ "$OS" = "linux" ]; then
        ARTIFACT="aria2-${ARCH}-linux"
        EXT="tar.gz"
    else
        ARTIFACT="aria2-${ARCH}-macos"
        EXT="tar.gz"
    fi
    
    info "Detected platform: ${OS}/${ARCH}"
}

# Determine installation directory
determine_prefix() {
    if [ -z "$PREFIX" ]; then
        # Try /usr/local/bin first, fall back to ~/.local/bin
        if [ -w "/usr/local/bin" ] || [ "$EUID" = "0" ]; then
            PREFIX="/usr/local/bin"
        else
            PREFIX="$HOME/.local/bin"
            # Create ~/.local/bin if it doesn't exist
            mkdir -p "$PREFIX"
        fi
    fi
    
    info "Installation directory: $PREFIX"
}

# Get latest version from GitHub API
get_latest_version() {
    local api_url="https://api.github.com/repos/${REPO}/releases/latest"
    local response
    
    response=$(curl -fsSL "$api_url" 2>/dev/null) || true
    
    if [ -n "$response" ]; then
        echo "$response" | grep -m 1 '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/'
    else
        echo ""
    fi
}

# Download and install
install() {
    local download_url
    local tmp_dir
    local archive_file
    
    # Resolve version
    if [ "$VERSION" = "latest" ]; then
        info "Fetching latest version..."
        VERSION=$(get_latest_version)
        if [ -z "$VERSION" ]; then
            warn "Could not determine latest version, using v0.1.0"
            VERSION="v0.1.0"
        fi
    fi
    
    info "Installing aria2-rust ${VERSION}"
    
    # Construct download URL
    download_url="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}.${EXT}"
    
    # Create temporary directory
    tmp_dir=$(mktemp -d)
    archive_file="${tmp_dir}/${ARTIFACT}.${EXT}"
    
    # Download
    info "Downloading from ${download_url}..."
    if ! curl -fsSL --progress-bar "$download_url" -o "$archive_file"; then
        error "Failed to download ${download_url}"
    fi
    
    # Extract
    info "Extracting..."
    tar -xzf "$archive_file" -C "$tmp_dir"
    
    # Find binary
    local binary_path="${tmp_dir}/${BINARY_NAME}"
    if [ ! -f "$binary_path" ]; then
        # Binary might be in a subdirectory
        binary_path=$(find "$tmp_dir" -name "$BINARY_NAME" -type f | head -n 1)
    fi
    
    if [ ! -f "$binary_path" ]; then
        error "Binary not found in archive"
    fi
    
    # Make executable
    chmod +x "$binary_path"
    
    # Install
    info "Installing to ${PREFIX}..."
    mv "$binary_path" "${PREFIX}/${BINARY_NAME}"
    
    # Cleanup
    rm -rf "$tmp_dir"
    
    # Verify installation
    if command -v aria2c &> /dev/null; then
        local installed_version
        installed_version=$("${PREFIX}/${BINARY_NAME}" --version 2>&1 | head -n 1 || echo "installed")
        success "aria2-rust installed successfully: ${installed_version}"
    else
        success "aria2-rust installed successfully to ${PREFIX}/${BINARY_NAME}"
        info "You may need to add ${PREFIX} to your PATH"
    fi
    
    # Show usage hint
    echo ""
    echo "Quick start:"
    echo "  aria2c http://example.com/file.zip"
    echo ""
    echo "For more information, run: aria2c --help"
}

# Main
main() {
    echo ""
    echo "  ___  _ __ ___ _ __ ___   __ _ _ __"
    echo " / _ \\| '__/ _ \\ '_ \` _ \\ / _\` | '_ \\"
    echo "| (_) | | |  __/ | | | | | (_| | | | |"
    echo " \\___/|_|  \\___|_| |_| |_|\\__,_|_| |_|"
    echo "        Rust Edition Installer"
    echo ""
    
    detect_platform
    determine_prefix
    install
}

main "$@"
