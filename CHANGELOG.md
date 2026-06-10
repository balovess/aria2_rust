# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-06-10

### Added
- Initial release of aria2-rust
- Core download engine with HTTP/HTTPS/FTP support
- BitTorrent protocol support with DHT, PEX, MSE
- Metalink support
- Magnet link support
- JSON-RPC/XML-RPC interface
- WebSocket support for real-time events
- Multi-connection parallel downloads
- Segment-based download with checksum verification
- Resume support for interrupted downloads
- Rate limiting and bandwidth control
- Proxy support (HTTP, SOCKS4, SOCKS5)
- Authentication (Basic, Digest)
- Cookie support
- Configuration file parsing
- Python SDK (`aria2-rust-client`)
- Node.js SDK (`@aria2-rust/client`)
- Homebrew formula for macOS
- Scoop manifest for Windows
- Docker support
- Comprehensive test suite

### Security
- TLS/SSL support for HTTPS downloads
- Encrypted BitTorrent connections (MSE)
