# Compatibility Status

Last verified: 2026-08-08  
Reference implementation: aria2_original/  
Rust workspace version: 0.2.8

This is the current status source for the migration. The file-level records
under docs/migration/ and the historical plans under .trae/ are useful
evidence, but their checklists do not establish behavioural compatibility.

## Status Rules

| Status | Meaning |
| --- | --- |
| FULL | Rust covers the relevant original behaviour and has focused protocol or end-to-end evidence. |
| PARTIAL | The main path exists, but a documented behaviour, lifecycle, platform, or interoperability gap remains. |
| UNVERIFIED | Code exists, but the required protocol/E2E evidence has not been run or is not reproducible on the current host. |
| MISSING | No Rust equivalent exists for an in-scope original capability. |
| N/A | The original implementation is replaced by an intentional Rust/platform mechanism and the replacement is documented. |

“Source file compared” is not the same claim as FULL. The 530 C++ source
units catalogued in docs/MIGRATION.md measure audit coverage only.

## Current Module Matrix

| Area | Rust implementation | Status | Main evidence or remaining gap |
| --- | --- | --- | --- |
| Engine and scheduling | aria2-core/src/engine/ | PARTIAL | Typed command loop, generation-based completion accounting, `CancellationToken` shutdown, pause/unpause requeueing, runtime concurrency and global rate updates are covered. Full parity across retry, allocation, and all protocol commands is not yet proven. |
| HTTP/HTTPS | aria2-core/src/http/, aria2-protocol/src/http/ | PARTIAL | Focused parser and download coverage exists. Core owns production orchestration; `aria2-protocol::http::client` remains a separate compatibility-layer client with no core production callers, and broader original-binary interoperability remains unverified. |
| FTP/FTPS | aria2-core/src/ftp/, aria2-protocol/src/ftp/ | PARTIAL | Active/passive/auth paths exist; platform and live-server coverage is incomplete. |
| SFTP | aria2-protocol/src/sftp/, aria2-core/src/engine/sftp_download_command/ | UNVERIFIED | `ssh-host-key-md` fingerprint pinning and mismatch rejection are implemented and unit-tested; no live SFTP server E2E evidence on the current matrix. Known-hosts persistence is not part of aria2_original's `ssh-host-key-md` contract. |
| BitTorrent | aria2-protocol/src/bittorrent/, aria2-core/src/engine/bt_* | PARTIAL | Core protocol pieces exist; incoming listener ownership, dependency graph, and full scheduler/seeding parity remain. |
| DHT and trackers | aria2-protocol/src/bittorrent/dht/ | PARTIAL | Production paths and tests now use the protocol crate as the single canonical DHT implementation; the former `aria2-core/src/dht/` source tree is no longer exported. Complete live-network evidence is still missing. |
| Metalink | aria2-protocol/src/metalink/, aria2-core/src/engine/metalink_* | PARTIAL | V3/V4 parsing, filtering, resource downloads, manager-owned GID allocation, relative-URI base propagation, and metadata/payload graph terminal states have focused regression coverage. Same-metaurl multi-file grouping, full `follow-torrent=mem` semantics, session graph restoration, and live protocol interoperability remain open. |
| Integrity and resume | aria2-core/src/checksum/, aria2-core/src/session/ | PARTIAL | Main checksum and session paths work; integrity entry lifecycle, piece storage callbacks, and tail cleanup still contain deferred paths. |
| RPC and WebSocket | aria2-rpc/src/ | PARTIAL | JSON-RPC/XML-RPC/WebSocket surfaces, token/Basic authentication, aria2-compatible error/status mapping, and real HTTP E2E coverage exist. `cargo test -p aria2-rpc --all-features --tests -- --test-threads=1` passes 372 tests. Full original-client interoperability, including the browser-extension matrix, remains unverified. |
| CLI and options | aria2/src/app/, aria2-core/src/config/ | PARTIAL | Core parsing exists; enum choice validation and the runtime global-change policy now have a canonical core seam, but the complete option inventory/default/changeability parity still requires generated comparison and E2E proof. |
| Public C API/ABI | aria2_original/src/aria2api.cc, src/includes/aria2/aria2.h | PARTIAL | `aria2-core/src/c_api.rs` and `bindings/c/` provide a tested opaque-handle `extern "C"`/cdylib migration interface. It is intentionally source-level and is not binary-compatible with the original C++ classes or STL ABI. |

## Verification Evidence

Verified on 2026-08-08 with single-job builds where needed:

~~~text
cargo fmt --all -- --check                              PASS
cargo check -p aria2-core --all-features --tests -j 1    PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings PASS
cargo clippy --workspace --all-targets --all-features -- -D warnings PASS
cargo test -p aria2-core --all-features c_api --lib   PASS
cargo test -p aria2-protocol --all-features -j 1        872 passed, 4 ignored
cargo test -p aria2-rpc --all-features --tests -- --test-threads=1 372 passed, 0 failed
cargo test -p aria2 --all-features -j 1                 254 passed, 5 ignored
cargo build -p aria2 --all-features -j 1                PASS
npm run typecheck                                        PASS
npm run build                                            PASS
ARIA2_RUST_BIN=target/debug/aria2c.exe npm test          123 passed
PYTHONPATH=.codex-python-deps python -m pytest ...       136 passed
~~~

Latest RPC compatibility checkpoint (2026-08-08): unknown and non-changeable
options are ignored as in `aria2_original`; a recognized option with an
invalid value returns execution error `code=1` and HTTP 400 for JSON-RPC.
The checkpoint includes 206 library tests, 17 integration tests, and all
HTTP/WebSocket/XML-RPC/stress targets in the 372-test command above. The
active/reserved task changeability policy is centralized with the global
policy in `aria2-core/src/config/runtime.rs`; `request_group` only preserves
the historical re-export path. This is a RPC-scope result only; it does not
establish workspace-wide completion or full compatibility with every original
browser client.

The core all-features test targets executed 3,411 tests with 11 ignored and
0 failures before the Windows 600-second command limit expired while the
final target was being started. `test_e2e_bittorrent_download` was then run
separately: 21 passed, 2 ignored, 0 failed. The aggregate workspace command
`cargo test --workspace --all-features -j 1` also exceeded the Windows build
timeout before entering its test phase, so this document does not claim one
green workspace aggregate run.

The current remaining acceptance gaps are:

- The original C++ class/STL ABI is not and cannot be claimed as binary
  compatible; the Rust project currently provides a separate opaque-handle
  source-level C migration ABI. Header/API coverage beyond the current
  session, control, and snapshot surface is still incomplete.
- Metalink dependency lifecycle now has explicit metadata-success,
  direct-mirror-fallback, and terminal-failure states. Same-metaurl
  multi-file grouping, `follow-torrent=mem`, session graph restoration, and
  real HTTP/FTP/SFTP/DHT/BitTorrent Metalink interoperability still need
  implementation or reproducible evidence.
- HTTP, FTP, and DHT still have multiple layers with incomplete canonical
  ownership; live SFTP/FTP/DHT interoperability is not reproducible here.
- CLI/config defaults, changeability, error semantics, and the complete
  original-client matrix still need generated comparison and E2E proof.
- Some network-oriented tests remain intentionally ignored; ignored tests are
  not counted as compatibility evidence.
- No comparable aria2 C++ performance baseline has been recorded. Rust-only
  benchmark results are regression evidence, not proof of superiority.

## Architecture And Duplication Register

The current refactoring uses these module seams and records the remaining
consolidation work explicitly:

| Concern | Canonical seam | Decision / next action |
| --- | --- | --- |
| Engine command creation | `aria2-core/src/engine/task_spawner.rs` | Keep protocol selection and construction behind this deep module; the engine loop only owns lifecycle accounting and admission. |
| Global bandwidth limits | `RateLimiter` shared through `Arc` | One token-bucket state is shared by active and future commands; RPC updates also refresh `RequestGroupMan`'s reporting snapshot. |
| DHT | `aria2-protocol/src/bittorrent/dht/` | Keep the protocol crate as the canonical implementation; do not revive the unexported duplicate core tree. |
| RPC option parsing | `aria2-core/src/config/runtime.rs`, `config/option/types.rs`, and `request/request_group/options_ops.rs` | Keep original runtime changeability, enum choices, and typed string/size/integer/boolean parsing in core. RPC handlers normalize transport values, select the core policy, and map parse failures to aria2 execution errors; do not add another option whitelist or parser in the RPC crate. |
| HTTP/FTP transport | core orchestration plus protocol transport | Existing layers are useful adapters but are not yet one canonical implementation; remove pass-through duplication only after behavior comparison and live interoperability coverage. |
| Integrity | core streaming/control-file paths plus Metalink verifier | Preserve separate algorithm-specific adapters for now; unify lifecycle callbacks after the remaining TODO/no-op paths have regression coverage. |

## Acceptance Gate

The external compatibility boundary is strict: RPC/JSON-RPC/XML-RPC/WebSocket
wire shapes, authentication, parameters, errors, HTTP status codes, and
observable lifecycle behavior must match `aria2_original`. Internal Rust
architecture and performance are free to improve only behind that boundary.

The migration is not complete until the matrix is backed by reproducible
tests for default, BitTorrent, Metalink, SFTP, RPC, CLI, session/resume, and
binding workflows on the supported platforms. A green focused test or a
completed source comparison must not be reported as workspace all pass.

Performance claims also require a recorded benchmark protocol and comparable
aria2 C measurements. Rust-only benchmark results are regression evidence,
not proof of outperforming the original.
