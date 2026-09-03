# aria2-rust

中文：[`README_CN.md`](README_CN.md)

> **Version Notice:** aria2-rust is currently in a period of rapid iteration.
> Older versions may retain various issues and basic functionality is not
> guaranteed. Please use the latest version as soon as possible.

## Documentation

Start with the [documentation index](docs/README-en.md). The main user paths are:

- [Quick Start](docs/quickstart-en.md)
- [Configuration Guide](docs/configuration-guide-en.md)
- [RPC Guide](docs/rpc-guide-en.md)
- [Troubleshooting](docs/troubleshooting-en.md)

<p align="center">
  <strong>The ultra-fast download utility — rewritten in Rust</strong>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#usage">Usage</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#performance">Performance</a> •
  <a href="#building">Building</a> •
  <a href="#license">License</a>
</p>

---

**aria2-rust** is an independent Rust download engine. It provides practical
compatibility with the [aria2](https://aria2.github.io/) ecosystem so existing
users and tools can migrate easily, while its architecture, safety,
performance, and product direction are its own. The default build supports
HTTP/HTTPS, FTP, BitTorrent, and JSON-RPC/XML-RPC/WebSocket paths;
Metalink and SFTP require their Cargo features;
compatibility status and verification evidence are tracked in
[docs/compatibility-status.md](docs/compatibility-status.md).

## Implemented Capabilities

The capability inventory below describes code paths, not a claim that every
feature has passed the complete cross-platform E2E matrix. See the
[compatibility status](docs/compatibility-status.md) for the current gate.

- **Multi-Protocol Download**: HTTP/HTTPS, FTP, and BitTorrent by default; SFTP and Metalink are feature-gated
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
- **Configuration System**: Typed option registry with four-source merging (CLI/file/environment/defaults)
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
| Homebrew (macOS/Linux) | Formula draft only; tap and stable automated updates are not yet available |
| Scoop (Windows) | Experimental manifest; stable Windows x64 releases are checked in CI |
| Cargo (from source) | Supported: `cargo install --path aria2` |

Homebrew and Scoop packaging are distribution work in progress and are not a
current priority. For reliable installation, use the platform installer
scripts or the binaries attached to a GitHub Release. Do not treat the local
Homebrew formula or Scoop manifest as a compatibility or release guarantee.

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

### Initialize Persistent Paths

Run `aria2c --init` to choose system, current-directory, executable-directory,
portable, or custom storage. Existing configuration is backed up before reset.
Use `--non-interactive` with an explicit profile in automation:

```bash
aria2c --init
aria2c --init --profile=system --non-interactive
aria2c --init --profile=custom --state-dir="$HOME/.aria2" --download-dir="$HOME/Downloads" --non-interactive
aria2c --show-paths --profile=system
```

The generated configuration is intentionally minimal. Session, logging, PID,
cookies, DHT state, and server statistics are enabled only by explicit options.

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
| [windows.conf](examples/configs/windows.conf) | Windows manual configuration template |

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

For common commands, configuration syntax, RPC/daemon setup, sessions, and
configuration check/repair/reset workflows, see the
[user guide](docs/user-guide.md).

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
│   │   ├── process_wait.rs # Native process-exit events with fallback watcher
│   │   ├── download_engine.rs # Event loop with command queue
│   │   ├── download_command.rs # HTTP/HTTPS downloader
│   │   ├── ftp_download_command.rs # FTP/SFTP downloader
│   │   ├── bt_download_command.rs # BitTorrent downloader
│   │   ├── magnet_download_command.rs # Magnet link downloader
│   │   ├── metalink_download_command.rs # Metalink downloader
│   │   └── concurrent_download_command.rs # Multi-segment downloader
│   ├── src/config/        #   Typed configuration registry and parser
│   │   ├── option.rs     #     OptionType/Value/Def/Registry
│   │   ├── parser.rs     #     Multi-source parser (CLI/file/env/defaults)
│   │   ├── netrc.rs      #     NetRC authentication parser
│   │   ├── uri_list.rs  #     URI list file (-i option) parser
│   │   └── mod.rs        #     ConfigManager unified runtime manager
│   ├── src/request/       #   Request management
│   │   ├── request_group_man.rs # Global task manager
│   │   └── request_group/      # Per-task state machine and activity signals
│   │       └── activity.rs     # Notify + generation wake-up signal
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
│   │   ├── auto_save_coordinator.rs # Unified persistence deadlines
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

## Performance

The implementation-oriented comparison with `aria2_original` is maintained in
[Performance Differentiators](docs/performance-differentiators.md). The current
Rust-specific differences are:

| Area | Current implementation |
| --- | --- |
| Disk I/O | Positioned offset writes, write-back range cache, threshold batching, and coalesced multi-file writes. Blocking syscalls run on Tokio's blocking pool; Linux `io_uring` is an opt-in backend. |
| Data path | `bytes::Bytes` is transferred through the cache, Piece writer, and multi-file slices to reduce copies and temporary allocations. This is a reduced-copy path, not an end-to-end zero-copy guarantee. |
| Hash verification | Bounded background hash workers, chunked integrity dispatch, cooperative yields, and RequestGroup-aware cancellation. |
| BitTorrent/DHT | Hash-based peer lifecycle, incremental piece-frequency tracking, shared HAVE frame encoding with bounded concurrent sends, bucket-tree/top-K routing, and bounded UDP workers. |
| File allocation | Platform-aware Linux `fallocate`, Windows `SetFileValidData`, macOS `F_PREALLOCATE`, and cooperative fallbacks that keep long allocation work off the reactor. |
| RPC control plane | Owned wire parsing, up to 64 concurrent read-only calls in HTTP/WebSocket batches, mutation barriers, and blocking workers for heavy payload conversion. `system.multicall` keeps original sequential semantics. |

### Current Version Compared with Original

The following data comes from a Release build and idle-RPC process measurement on the same Windows 11 x64 machine:

| Metric | Original aria2 1.37.0 | Current aria2-rust reference |
| --- | ---: | ---: |
| `aria2c.exe` file size | about 5.39 MiB | about 13.6 MiB |
| Idle RPC Working Set | about 12.5 MiB | about 16.1 MiB |
| Idle RPC Private Bytes | about 3.25 MiB | about 3.6 MiB |

These values vary with compiler features, Windows version, allocator, and measurement timing. They are not a substitute for a same-load download benchmark. Private Bytes are already close to the original, while the current binary size and Working Set remain higher.

The same document records compatibility impact, source entry points, focused test
evidence, and known boundaries. DHT network maintenance and active peer
lookups now use the unified task queue, with duplicate periodic network ticks
coalesced while a lane is busy. Token rotation and local cleanup remain in the
deadline coordinator, while routing-table saves use a blocking worker. Linux
and macOS runtime evidence and a comparable full-download workload benchmark
against `aria2_original` are still pending.

Existing event-driven hot paths include:

- uTP receive waits on Tokio UDP readiness and retains fragmented frames in a
  persistent buffer instead of retrying `recv` after a fixed sleep.
- BitTorrent piece downloads use a bounded event-driven block pipeline;
  endgame peers are consumed concurrently and TCP/MSE readers preserve partial
  frames across cancellation.
- Dynamic rate-limit changes wake blocked token acquisitions immediately.
- Piece-stat and missing-piece queries scan bitfields bytewise; unrestricted
  rarest-first selection advances a sorted cursor instead of rescanning pieces.
- The engine idle path waits on commands, task completions, the earliest
  maintenance deadline, and shutdown instead of scanning on a fixed idle tick.

Implementation seams: [engine idle wait](aria2-core/src/engine/engine_loop.rs),
[activity signal](aria2-core/src/request/request_group/activity.rs),
[save deadlines](aria2-core/src/session/auto_save_coordinator.rs), and
[platform process wait](aria2-core/src/engine/process_wait.rs).

Rust-only Criterion measurements on Windows release builds (`50,000` pieces,
same-worktree before/after medians) show the following algorithm-level changes:

| Benchmark | Before | After |
| --- | ---: | ---: |
| Bitfield all-missing query | 63.4 us | 4.3 us |
| Bitfield sparse selection | 194.9 us | 75.0 us |
| Rarest selection | 54.96 us | 2.13 us |

These are microbenchmark results, not a whole-download throughput claim or a
comparison with `aria2_original`. Details and validation commands are recorded
in [docs/MIGRATION.md](docs/MIGRATION.md) and
[docs/engine-loop-performance.md](docs/engine-loop-performance.md).

To reproduce the focused benchmarks:

```bash
cargo bench -p aria2-core --bench segment_scan_bench -- --noplot
cargo bench -p aria2-protocol --features bittorrent --bench sequential_picker_bench -- rarest_selection --noplot
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
