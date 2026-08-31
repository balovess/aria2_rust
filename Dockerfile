# aria2-rust Docker Image
# 
# Multi-stage build for minimal image size
# 
# Usage:
#   docker build -t aria2-rust .
#   docker run -d --name aria2 -p 6800:6800 -v ~/downloads:/downloads aria2-rust

# ============================================
# Stage 1: Build
# ============================================
FROM rust:1.95-alpine AS builder

# Install build dependencies
# openssl-libs-static is required because Rust's musl target links statically
# by default and needs the .a files (openssl-dev only provides headers + .so).
RUN apk add --no-cache \
    musl-dev \
    pkgconfig \
    openssl-dev \
    openssl-libs-static \
    git

WORKDIR /build

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY aria2 ./aria2
COPY aria2-core ./aria2-core
COPY aria2-protocol ./aria2-protocol
COPY aria2-rpc ./aria2-rpc

# Build release binary
RUN cargo build --release --package aria2

# ============================================
# Stage 2: Runtime
# ============================================
FROM alpine:3.19

LABEL maintainer="aria2-rust contributors"
LABEL description="aria2-rust - The ultra fast download utility (Rust edition)"
LABEL org.opencontainers.image.source="https://github.com/balovess/aria2_rust"

# Install runtime dependencies
RUN apk add --no-cache \
    ca-certificates \
    tzdata

# Create non-root user
RUN addgroup -g 1000 aria2 && \
    adduser -u 1000 -G aria2 -s /bin/sh -D aria2

# Copy binary from builder
COPY --from=builder /build/target/release/aria2c /usr/local/bin/aria2c

# Create directories
RUN mkdir -p /downloads /config && \
    chown -R aria2:aria2 /downloads /config

# Set environment
ENV HOME=/home/aria2

# Switch to non-root user
USER aria2
WORKDIR /downloads

# Default configuration
ENV RPC_LISTEN_PORT=6800

# Expose RPC port
EXPOSE 6800

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD wget -q --spider http://localhost:6800/jsonrpc || exit 1

# Volume for downloads and configuration/state. Initialize once with:
# docker run --rm -v ./config:/config -v ./downloads:/downloads image \
#   aria2c --init --profile=custom --state-dir=/config --download-dir=/downloads --non-interactive
VOLUME ["/downloads", "/config"]

# Entry point
ENTRYPOINT ["/usr/local/bin/aria2c"]

# Default command: start RPC server
CMD ["--enable-rpc=true", \
     "--rpc-listen-all=true", \
     "--rpc-listen-port=6800", \
     "--rpc-allow-origin-all=true", \
     "--dir=/downloads", \
     "--max-concurrent-downloads=5", \
     "--max-connection-per-server=16", \
     "--min-split-size=1M", \
     "--split=16", \
     "--continue=true", \
     "--auto-save-interval=60"]
