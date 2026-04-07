# aria2-rust

<p align="center">
  <strong>The ultra fast download utility — rewritten in Rust</strong>
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

aria2-rust 是知名下载工具 [aria2](https://aria2.github.io/) 的完整 Rust 重写版本。支持 HTTP/HTTPS、FTP/SFTP、BitTorrent、Metalink 协议，提供 JSON-RPC/XML-RPC/WebSocket 远程控制接口。

## Features

- **多协议下载**: HTTP/HTTPS、FTP/SFTP、BitTorrent (DHT/PEX/MSE)、Metalink V3/V4
- **多源镜像**: 自动从多个 URI 分段并行下载，最大化带宽利用率
- **断点续传**: 支持所有协议的断点续传，网络中断后无缝恢复
- **BitTorrent 完整支持**: DHT 网络、Tracker 通信、Peer 交换(PEX)、MSE 加密、阻塞算法
- **RPC 远程控制**: JSON-RPC 2.0、XML-RPC、WebSocket 实时事件推送
- **配置系统**: ~95 个核心选项，支持命令行 / 配置文件 / 环境变量四源合并
- **NetRC 认证**: 自动从 `.netrc` 文件读取 FTP/HTTP 凭证
- **URI 列表文件**: 支持 `-i` 参数批量导入下载任务

## Quick Start

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.70+ (stable)
- Windows / macOS / Linux

### Build & Run

```bash
# Clone the repository
git clone https://github.com/yourname/aria2-rust.git
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

The project is organized as a Cargo workspace with 4 crates:

```
aria2-rust/
├── aria2/                  # Binary crate (CLI entry point)
│   ├── src/main.rs        #   Entry point
│   ├── src/app.rs         #   App runtime (ConfigManager + Engine)
│   └── examples/          #   Usage examples
├── aria2-core/             # Core library (engine + config + UI)
│   ├── src/config/        #   Option registry, parser, ConfigManager
│   │   ├── option.rs     #     OptionType/Value/Def/Registry (~95 options)
│   │   ├── parser.rs     #     Multi-source parser (CLI/file/env/defaults)
│   │   ├── netrc.rs      #     NetRC authentication parser
│   │   ├── uri_list.rs  #     URI list file (-i option) parser
│   │   └── mod.rs        #     ConfigManager unified runtime manager
│   ├── src/engine/        #   Download engine
│   │   └── download_engine.rs # Event loop with command queue
│   ├── src/request/       #   Request management
│   │   ├── request_group_man.rs # Global task manager
│   │   └── request_group.rs    # Per-task state machine
│   ├── src/filesystem/     #   Disk I/O (adaptor/writer/cache/allocation)
│   └── src/ui.rs           #   Progress bar & status panel
├── aria2-protocol/         # Protocol library
│   ├── src/http/           #   HTTP/HTTPS client (auth/proxy/cookies/compression)
│   ├── src/ftp/            #   FTP/SFTP client (anonymous+auth, passive mode)
│   └── src/bittorrent/     #   BT protocol (bencode/torrent/DHT/tracker/peer/PEx/MSE)
├── aria2-rpc/              # RPC library
│   ├── src/json_rpc.rs     #   JSON-RPC 2.0 codec
│   ├── src/xml_rpc.rs      #   XML-RPC codec
│   ├── src/websocket.rs    #   WebSocket event publisher
│   ├── src/server.rs       #   HTTP server framework (auth/CORS/status models)
│   └── src/engine.rs       #   RpcEngine bridge (25 RPC methods)
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
| CLI arguments | ✅ Core | ~50 most-used options implemented |
| Configuration file (`aria2.conf`) | ✅ | Same syntax format |
| Environment variables | ✅ | `ARIA2_*` prefix mapping |
| JSON-RPC API | ✅ | 25 methods compatible |
| XML-RPC API | ✅ | Full methodCall/response/fault support |
| WebSocket events | ✅ | 7 event types |
| URI list file (`-i`) | ✅ | Mirror + inline options |
| NetRC auth | ✅ | machine/default/macdef parsing |
| Session save/load | ✅ | Round-trip consistent |
| Metalink V3/V4 | ✅ | Full parsing |
| BitTorrent DHT | ✅ | Bootstrap nodes + KRPC |
| FTP/SFTP | ✅ | Passive mode + auth |

**Not yet implemented** (planned for future):
- Magnet link support
- Cookie import/export (Firefox/Chrome format)
- Real-time speed graph (TUI)
- Full 300+ option coverage (currently ~95 core options)

## License

This project is licensed under **GPL-2.0-or-later**, consistent with the original [aria2](https://github.com/aria2/aria2) project.

Copyright (C) 2024 aria2-rust contributors.

## Acknowledgments

- [aria2](https://aria2.github.io/) — The original C++ download utility that inspired this project
- [Tokuio](https://tokio.rs/) — Async runtime for Rust
- [Reqwest](https://docs.rs/reqwest/) — HTTP client foundation
- [Axum](https://docs.rs/axum/) — Web framework for RPC server
