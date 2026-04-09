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

**aria2-rust** is a complete rewrite of the renowned [aria2](https://aria2.github.io/) download utility in Rust. It supports HTTP/HTTPS, FTP/SFTP, BitTorrent, and Metalink protocols, with JSON-RPC/XML-RPC/WebSocket remote control capabilities.

## Features

- **Multi-Protocol Download**: HTTP/HTTPS, FTP/SFTP, BitTorrent (DHT/PEX/MSE), Metalink V3/V4
- **Multi-Source Mirrors**: Automatic segmented parallel downloads from multiple URIs for maximum bandwidth utilization
- **Resume Support**: Breakpoint resume on all protocols with seamless recovery after network interruptions
- **Full BitTorrent Support**: 
  - ✅ DHT network (KRPC + routing table + bootstrap)
  - ✅ Tracker communication (UDP/HTTP)
  - ✅ Peer Exchange (PEX)
  - ✅ MSE/PE encryption (BEP14 handshake)
  - ✅ Choking algorithms + seed-time/ratio support
  - ✅ RarestFirst piece selection
- **Rate Limiting**: Token bucket algorithm with per-task/global limits
- **Cookie Management**: Netscape format persistence + auto-loading from files
- **Session Management**: Auto-save + manual save/load with .aria2 control files
- **RPC Remote Control**: JSON-RPC 2.0, XML-RPC, WebSocket (25 methods + 7 events)
- **Configuration System**: ~95 core options with four-source merging (CLI/file/environment/defaults)
- **NetRC Authentication**: Automatic FTP/HTTP credential loading from `.netrc` files
- **URI List Files**: Batch import download tasks via `-i` parameter
- **Public Tracker List**: Auto-update from trackerslist.com for BT peer discovery

## Quick Start

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.70+ (stable)
- Windows / macOS / Linux

### Build & Run

```bash
# Clone the repository
git clone https://github.com/aria2/aria2-rust.git
cd aria2-rust

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

## Usage

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
| `-s, --split` | Connections per server | `1` |
| `-x, --max-connection-per-server` | Max connections per server | `1` |
| `--max-download-limit` | Max download speed | unlimited |
| `--timeout` | Timeout in seconds | `60` |
| `-q, --quiet` | Quiet mode | false |

### BitTorrent Download

```bash
aria2c file.torrent
```

### URI List File

Create a text file with URIs (one entry per block, Tab-separated mirrors):

```
  dir=/downloads
  split=5
http://mirror1.example.com/file.iso	http://mirror2.example.com/file.iso
http://mirror3.example.com/file.iso
```

Then:

```bash
aria2c -i uris.txt
```

## Architecture

Total Codebase: ~14,500+ lines Rust/TS  
Test Suite: ~300+ tests passing

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

## Compatibility with Original aria2

| Feature | Status | Notes |
|---------|--------|-------|
| CLI arguments | ✅ Core | ~50 most-used options |
| Configuration file (`aria2.conf`) | ✅ | Same syntax format |
| Environment variables | ✅ | `ARIA2_*` prefix mapping |
| JSON-RPC API | ✅ | 25 methods compatible |
| XML-RPC API | ✅ | Full methodCall/response/fault support |
| WebSocket events | ✅ | 7 event types |
| URI list file (`-i`) | ✅ | Mirror + inline options |
| NetRC auth | ✅ | machine/default/macdef parsing |
| Session save/load | ✅ | Round-trip consistent |
| Metalink V3/V4 | ✅ | Full parsing |
| BitTorrent DHT | ✅ | KRPC + routing table + bootstrap |
| FTP/SFTP | ✅ | Passive mode + auth |
| Rate limiting | ✅ | Token bucket algorithm |
| Cookie management | ✅ | Netscape format persistence |
| MSE/PE encryption | ✅ | BEP14 handshake |
| Magnet link support | ✅ | ut_metadata fetching |
| RarestFirst piece | ⚠️ Partial | Basic implementation |

**Not yet implemented** (planned for future):
- Real-time speed graph (TUI)
- Full 300+ option coverage (currently ~95 core options)
- Endgame mode for BitTorrent
- DHT routing table persistence (dht.dat)

## License

This project is licensed under **GPL-2.0-or-later**, consistent with the original [aria2](https://github.com/aria2/aria2) project.

Copyright (C) 2024 aria2-rust contributors.

## Acknowledgments

- [aria2](https://aria2.github.io/) — The original C++ download utility that inspired this project
- [Tokio](https://tokio.rs/) — Async runtime for Rust
- [Reqwest](https://docs.rs/reqwest/) — HTTP client foundation
- [Axum](https://docs.rs/axum/) — Web framework for RPC server
