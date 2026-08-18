# aria2-rust

<p align="center">
  <strong>The ultra-fast download utility — rewritten in Rust</strong>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#usage">Usage</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#building">Building</a> •
  <a href="#license">License</a>
</p>

---

**aria2-rust** is an independent Rust download engine. It provides practical
compatibility with the [aria2](https://aria2.github.io/) ecosystem so existing
users and tools can migrate easily, while its architecture, safety,
performance, and product direction are its own. It supports HTTP/HTTPS,
FTP/SFTP, BitTorrent, Metalink, and JSON-RPC/XML-RPC/WebSocket paths;
compatibility status and verification evidence are tracked in
[docs/compatibility-status.md](docs/compatibility-status.md).

## Implemented Capabilities

The capability inventory below describes code paths, not a claim that every
feature has passed the complete cross-platform E2E matrix. See the
[compatibility status](docs/compatibility-status.md) for the current gate.

- **Multi-Protocol Download**: HTTP/HTTPS, FTP/SFTP, BitTorrent (DHT/PEX/MSE), Metalink V3/V4
- **Multi-Source Mirrors**: Automatic segmented parallel downloads from multiple URIs for maximum bandwidth utilization
- **Resume Support**: Breakpoint resume on all protocols with seamless recovery after network interruptions
- **Full BitTorrent Support**: 
  - ✅ DHT network (KRPC + routing table + bootstrap)
  - ✅ Tracker communication (UDP/HTTP)
  - ✅ Peer Exchange (PEX, per-peer BEP 10 extension-ID negotiation)
  - ✅ MSE/PE encryption (BEP14 handshake)
  - ✅ Choking algorithms + seed-time/ratio support
  - ✅ RarestFirst piece selection
  - ✅ uTP protocol (BEP 29) - Not in original aria2 C++
  - ✅ Web Seeds (BEP 19)
  - ✅ LPD (Local Peer Discovery)
  - ✅ Complete seeding mode with upload support
- **Rate Limiting**: Token bucket algorithm with per-task/global limits
- **Cookie Management**: Netscape format persistence + auto-loading from files
- **Session Management**: Auto-save + manual save/load with .aria2 control files
- **RPC Remote Control**: JSON-RPC 2.0, XML-RPC, WebSocket (36 all-features methods, 6 notifications; compatibility coverage tracked separately)
- **Configuration System**: ~95 core options with four-source merging (CLI/file/environment/defaults)
- **NetRC Authentication**: Automatic FTP/HTTP credential loading from `.netrc` files
- **URI List Files**: Batch import download tasks via `-i` parameter
- **Public Tracker List**: Auto-update from trackerslist.com for BT peer discovery

## Quick Start

### One-Line Installation (Recommended)

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/balovess/aria2_rust/main/install.sh | bash
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/balovess/aria2_rust/main/install.ps1 | iex
```

**Docker:**
```bash
docker run -d --name aria2 -p 6800:6800 -v ~/downloads:/downloads ghcr.io/balovess/aria2-rust:latest
```

**Package Managers:**

| Platform | Command |
|----------|---------|
| Homebrew (macOS/Linux) | `brew install ./homebrew/aria2-rust.rb` |
| Scoop (Windows) | `scoop install ./scoop/aria2-rust.json` |
| Cargo (from source) | `cargo install --path aria2` |

### First Download

After installation, start downloading immediately:

```bash
# Download a file
aria2c http://example.com/file.zip

# Download with multiple connections
aria2c -x 16 -s 16 http://example.com/large.iso

# Download a torrent
aria2c file.torrent

# Download with custom directory
aria2c -d ~/downloads http://example.com/file.zip
```

### Build from Source

<details>
<summary>Click to expand build instructions</summary>

**Prerequisites:**
- [Rust](https://www.rust-lang.org/tools/install) 1.70+ (stable)
- Windows / macOS / Linux

**Build Commands:**
```bash
# Clone the repository
git clone https://github.com/balovess/aria2_rust.git
cd aria2_rust

# Build all crates
cargo build --release

# Download a file (HTTP)
cargo run --release -- http://example.com/file.zip

# Download with custom options
cargo run --release -- -d ./downloads -s 4 http://example.com/large.iso

# Show help
cargo run --release -- --help

# Show version
cargo run --release -- --version
```

</details>

## Usage

### Configuration Templates

We provide ready-to-use configuration templates in `examples/configs/`:

| Template | Description |
|----------|-------------|
| [minimal.conf](examples/configs/minimal.conf) | Minimal configuration for quick setup |
| [basic.conf](examples/configs/basic.conf) | Basic configuration with common options |
| [advanced.conf](examples/configs/advanced.conf) | Advanced configuration with RPC, proxy, etc. |
| [bittorrent.conf](examples/configs/bittorrent.conf) | Optimized for BitTorrent downloads |

**Usage:**
```bash
# Copy template to config directory
mkdir -p ~/.aria2
cp examples/configs/basic.conf ~/.aria2/aria2.conf

# Edit as needed
nano ~/.aria2/aria2.conf

# Run with configuration
aria2c --conf-path=~/.aria2/aria2.conf http://example.com/file.zip
```

### Basic HTTP Download

```bash
aria2c http://example.com/file.zip
```

### With Options

```bash
aria2c -o output.dat -d /downloads -s 4 -x 8 http://example.com/large.bin
```

| Option | Description | Default |
|--------|-------------|---------|
| `-d, --dir` | Save directory | `.` |
| `-o, --out` | Output filename | auto |
| `-s, --split` | Concurrent segment requests per download | `16` |
| `-x, --max-connection-per-server` | Per-authority HTTP segment request cap; adaptive download may lower it | `16` |
| `--max-download-limit` | Max download speed | unlimited |
| `--timeout` | Timeout in seconds | `60` |
| `-q, --quiet` | Quiet mode | false |

For segmented HTTP downloads, `split` is the total concurrent request budget
for one file. `max-connection-per-server` is the independent cap for each
`scheme://host:port` authority. With multiple mirrors, different authorities
may run concurrently within the `split` budget; mirrors sharing one authority
share that authority's cap. HTTP adaptive concurrency starts at the configured
cap and lowers only the authority that returns `429` or `503`.
### BitTorrent Download

```bash
aria2c file.torrent
```

### URI List File

Create a text file with URIs (one entry per block, Tab-separated mirrors):

```
  dir=/downloads
  split=16
http://mirror1.example.com/file.iso	http://mirror2.example.com/file.iso
http://mirror3.example.com/file.iso
```

Then:

```bash
aria2c -i uris.txt
```

## Architecture

The workspace contains four Rust crates plus Python and Node.js bindings.
Test status is reported from reproducible commands in
[docs/compatibility-status.md](docs/compatibility-status.md), rather than as
a fixed historical test count.

Verification snapshot (2026-08-18): the current focused evidence includes
the CLI, RPC, protocol, BitTorrent, Metalink, FTP/SFTP, Node.js, and Python
regression/E2E suites recorded in
[docs/compatibility-status.md](docs/compatibility-status.md). The workspace
command `cargo test --workspace --all-targets --no-run` also compiles all Rust
test and benchmark targets. Node.js reports 123 passed and Python reports 137
passed on this host. Under the current `0.3.2`, the version entry point,
application tests, Clippy, and the targeted parser/RPC regressions pass;
`aria2c --version` reports `aria2-rust 0.3.2`, and all Rust members and SDK
metadata resolve to that product version. Existing `check-certificate`,
`ca-certificate`, `certificate`, and `private-key` configuration values are
handled by the Rust HTTP transport across primary HTTP/HTTPS downloads,
Metalink HTTP, production BitTorrent HTTP trackers, and web seeds; local
Rustls HTTPS fixtures verify custom CA trust, disabled verification, separate
PEM mutual TLS, legacy empty-password PKCS#12, and modern PBES2/AES-256
single-file identities;
aggregate runtime coverage,
platform-specific binding runs, complete original-client/browser-extension
interoperability, public C ABI compatibility, and the aria2 C performance
comparison remain open; these results do not mean the migration is complete.

The project is organized as a Cargo workspace with 4 crates:

```
aria2-rust/
├── aria2/                  # Binary crate (CLI entry point, ~550 lines)
│   ├── src/main.rs        #   Entry point
│   ├── src/app.rs         #   App runtime (ConfigManager + Engine)
│   └── examples/          #   Usage examples
├── aria2-core/             # Core library (~7,000 lines)
│   ├── src/engine/        #   Download engine (12 command implementations)
│   │   ├── download_engine.rs # Event loop with command queue
│   │   ├── download_command.rs # HTTP/HTTPS downloader
│   │   ├── ftp_download_command.rs # FTP/SFTP downloader
│   │   ├── bt_download_command.rs # BitTorrent downloader
│   │   ├── magnet_download_command.rs # Magnet link downloader
│   │   ├── metalink_download_command.rs # Metalink downloader
│   │   └── concurrent_download_command.rs # Multi-segment downloader
│   ├── src/config/        #   Configuration system (~95 options)
│   │   ├── option.rs     #     OptionType/Value/Def/Registry
│   │   ├── parser.rs     #     Multi-source parser (CLI/file/env/defaults)
│   │   ├── netrc.rs      #     NetRC authentication parser
│   │   ├── uri_list.rs  #     URI list file (-i option) parser
│   │   └── mod.rs        #     ConfigManager unified runtime manager
│   ├── src/request/       #   Request management
│   │   ├── request_group_man.rs # Global task manager
│   │   └── request_group.rs    # Per-task state machine
│   ├── src/filesystem/     #   Disk I/O
│   │   ├── disk_writer.rs # Disk writer trait
│   │   ├── disk_cache.rs # Cached writer (256KB direct write)
│   │   ├── control_file.rs # .aria2 control file format
│   │   ├── file_allocation.rs # Pre-allocation strategies
│   │   └── checksum.rs # Checksum verification
│   ├── src/http/          #   Cookie management
│   │   ├── cookie.rs # Cookie structure
│   │   ├── cookie_storage.rs # Persistent storage
│   │   └── ns_cookie_parser.rs # Netscape format parser
│   ├── src/session/       #   Session persistence
│   │   ├── session_serializer.rs # Serialization
│   │   ├── auto_save_session.rs # Auto-save
│   │   └── save_session_command.rs # Save on exit
│   ├── src/rate_limiter.rs # Token bucket rate limiting
│   └── src/ui.rs           #   Progress bar & status panel
├── aria2-protocol/         # Protocol stack (~5,000 lines)
│   ├── src/http/           #   HTTP/HTTPS client (auth/proxy/cookies/compression)
│   ├── src/ftp/            #   FTP/SFTP client (anonymous+auth, passive mode)
│   ├── src/bittorrent/     #   Full BT stack
│   │   ├── bencode/ # BEP3 bencode codec
│   │   ├── torrent/ # .torrent parsing
│   │   ├── magnet.rs # Magnet link parsing
│   │   ├── dht/ # KRPC + routing table + bootstrap
│   │   ├── tracker/ # UDP/HTTP tracker
│   │   ├── peer/ # Peer connection + handshake
│   │   ├── extension/ # MSE/PEX/ut_metadata
│   │   └── piece/ # Piece manager + picker
│   └── src/metalink/      #   Metalink V3/V4 parser
├── aria2-rpc/              # RPC server (~1,000 lines)
│   ├── src/json_rpc.rs     #   JSON-RPC 2.0 codec
│   ├── src/xml_rpc.rs      #   XML-RPC codec
│   ├── src/websocket.rs    #   WebSocket event publisher
│   ├── src/server.rs       #   HTTP server (auth/CORS/status)
│   └── src/engine.rs       #   RpcEngine bridge (25 RPC methods)
└── bindings/               # Language bindings (~1,200 lines)
    ├── python/            #   Python SDK (~600 lines)
    └── nodejs/            #   Node.js SDK (~627 lines TS)
└── Cargo.toml              # Workspace configuration
```

## Library Usage

### As a library in your Rust project

Add to your `Cargo.toml`:

```toml
[dependencies]
aria2-core = { path = "../aria2-core" }
aria2-rpc = { path = "../aria2-rpc" }
```

#### Minimal download example

```rust
use aria2_core::config::ConfigManager;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::config::OptionValue;

#[tokio::main]
async fn main() {
    let mut config = ConfigManager::new();
    config.set_global_option("dir", OptionValue::Str("./downloads".into())).await.unwrap();
    config.set_global_option("split", OptionValue::Int(4)).await.unwrap();

    let man = RequestGroupMan::new();
    let opts = DownloadOptions {
        split: Some(4),
        ..Default::default()
    };

    match man.add_group(vec!["http://example.com/file.zip".into()], opts).await {
        Ok(gid) => println!("Download started: #{}", gid.value()),
        Err(e) => eprintln!("Error: {}", e),
    }
}
```

#### RPC server example

```rust
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;

#[tokio::main]
async fn main() {
    let engine = RpcEngine::new();

    let req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.addUri".into(),
        params: serde_json::json!([["http://example.com/file.zip"]]),
        id: Some(serde_json::Value::String("req-1".into())),
    };

    let resp = engine.handle_request(&req).await;
    println!("{}", serde_json::to_string_pretty(&resp).unwrap());
}
```

## Building from Source

### Requirements

- **Rust**: 1.70 or later ([install](https://rustup.rs/))
- **OS**: Windows 10+, macOS 10.15+, Linux (glibc 2.17+)

### Build Commands

```bash
# Debug build (fast compilation)
cargo build

# Release build (optimized)
cargo build --release

# Run tests
cargo test --workspace

# Generate documentation
cargo doc --workspace --no-deps

# Run a specific example
cargo run --example simple_download -- http://example.com/test.bin
```

## Testing

### Running Tests

```bash
# Run all tests in workspace
cargo test --workspace

# Run tests for specific crate
cargo test -p aria2-core

# Run tests with verbose output
cargo test --workspace -- --nocapture

# Run specific test category
cargo test "test_e2e"      # E2E tests
cargo test "test_stress"   # Stress tests
cargo test "test_edge"     # Edge case tests
cargo test "test_error"    # Error path tests
```

### Test Categories

| Category | Prefix | Description |
|----------|--------|-------------|
| Unit Tests | `test_` | Inline tests for individual functions |
| Integration Tests | `test_` | Module interaction tests |
| E2E Tests | `test_e2e_` | Complete workflow tests |
| Stress Tests | `test_stress_` | High-load stability tests |
| Edge Case Tests | `test_edge_` | Boundary condition tests |
| Error Path Tests | `test_error_` | Error handling tests |

### Coverage Report

```bash
# Install cargo-tarpaulin (Linux/macOS)
cargo install cargo-tarpaulin

# Generate HTML coverage report
cargo tarpaulin --workspace --out Html --output-dir coverage/

# Generate LCOV format for CI
cargo tarpaulin --workspace --out Lcov --output-dir coverage/
```

### Running Benchmarks

```bash
# Run all benchmarks
cargo bench --workspace

# Run specific benchmark
cargo bench --bench config_bench
```

For comprehensive testing guidance, see [docs/testing-guide.md](docs/testing-guide.md).

## Compatibility with Original aria2

The table below records implemented code paths, not full compatibility. The
authoritative status is the module matrix in
[docs/compatibility-status.md](docs/compatibility-status.md); an implemented
path can still be `PARTIAL` or `UNVERIFIED` there.

| Feature | Path state | Notes |
|---------|--------|-------|
| CLI arguments | Implemented path | ~50 most-used options; full option parity is still open |
| Configuration file (`aria2.conf`) | Implemented path | Same syntax path; defaults and changeability still need comparison |
| Environment variables | Implemented path | `ARIA2_*` prefix mapping; full parity is still open |
| JSON-RPC API | Implemented path | 36 all-features methods returned by `system.listMethods`; interoperability remains open |
| XML-RPC API | Implemented path | MethodCall/response/fault paths exist; original-client matrix remains open |
| WebSocket events | Implemented path | 6 notifications returned by `system.listNotifications` |
| URI list file (`-i`) | Implemented path | Mirror + inline options |
| NetRC auth | Implemented path | machine/default/macdef parsing |
| Session save/load | Implemented path | Round-trip tests exist; complete control-file parity remains open |
| Metalink V3/V4 | Implemented path | Parsing and downloads exist; torrent metaurl lifecycle is partial |
| BitTorrent DHT | Implemented path | KRPC + routing table + bootstrap; live interoperability remains open |
| FTP/SFTP | Implemented path | Passive mode + auth; live-server evidence remains open |
| Rate limiting | Implemented path | Shared token bucket and runtime updates are tested |
| Cookie management | Implemented path | Netscape and SQLite parsing paths exist |
| MSE/PE encryption | Implemented path | BEP14 handshake |
| Magnet link support | Implemented path | ut_metadata fetching |
| RarestFirst piece | Implemented path | Piece selection implementation and tests |
| Endgame mode | Implemented path | Last-piece optimization |
| DHT persistence | Implemented path | `dht.dat` serialization |
| uTP Protocol | Extension path | Not in original aria2 C++ |
| Web Seeds | Implemented path | BEP 19 |
| LPD | Implemented path | Local Peer Discovery |
| Seeding Mode | Implemented path | Upload support |

**Known gaps and verification status:**
- This table is a capability inventory, not a release compatibility claim.
- The migration is not currently all pass; optional features, ignored network
  tests, platform-specific binding runs, original-client interoperability, and
  the public C ABI remain tracked work.
- `aria2.forceShutdown`, `system.listMethods`, and `system.listNotifications` are implemented and covered by handler/integration tests.
- HTTPS RPC has TLS configuration, server implementation, and dedicated test coverage; broader client/server interoperability testing remains tracked.
- IPv6 DHT has CLI and protocol support; full network interoperability coverage remains tracked.
- Additional CLI/runtime option behavior still requires systematic comparison against `aria2_original`.

## License

This project is licensed under **GPL-2.0-or-later**, consistent with the original [aria2](https://github.com/aria2/aria2) project.

Copyright (C) 2024 aria2-rust contributors.

## Acknowledgments

- [aria2](https://aria2.github.io/) — The original C++ download utility that inspired this project
- [Tokio](https://tokio.rs/) — Async runtime for Rust
- [Reqwest](https://docs.rs/reqwest/) — HTTP client foundation
- [Axum](https://docs.rs/axum/) — Web framework for RPC server
