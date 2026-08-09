# Compatibility Status

Last verified: 2026-08-09
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
| HTTP/HTTPS | aria2-core/src/http/, aria2-protocol/src/http/ | PARTIAL | Focused parser and download coverage exists, including existing-file naming, control-file cleanup, preallocation-safe resume recovery, multi-URI resume failover, and HTTP 200 responses that ignore a requested Range (`CannotResume` by default or fresh restart according to `always-resume`/`max-resume-failure-tries`). Core owns production orchestration; `aria2-protocol::http::client` remains a separate compatibility-layer client with no core production callers, and broader original-binary interoperability remains unverified. |
| FTP/FTPS | aria2-core/src/ftp/, aria2-protocol/src/ftp/ | PARTIAL | Active/passive/auth paths exist; platform and live-server coverage is incomplete. |
| SFTP | aria2-protocol/src/sftp/, aria2-core/src/engine/sftp_download_command/ | UNVERIFIED | `ssh-host-key-md` fingerprint pinning and mismatch rejection are implemented and unit-tested; no live SFTP server E2E evidence on the current matrix. Known-hosts persistence is not part of aria2_original's `ssh-host-key-md` contract. |
| BitTorrent | aria2-protocol/src/bittorrent/, aria2-core/src/engine/bt_* | PARTIAL | Core protocol pieces exist; incoming listener ownership, dependency graph, and full scheduler/seeding parity remain. |
| DHT and trackers | aria2-protocol/src/bittorrent/dht/ | PARTIAL | Production paths and tests now use the protocol crate as the single canonical DHT implementation; the former `aria2-core/src/dht/` source tree is no longer exported. Complete live-network evidence is still missing. |
| Metalink | aria2-protocol/src/metalink/, aria2-core/src/engine/metalink_* | PARTIAL | V3/V4 parsing, filtering, resource downloads, manager-owned GID allocation, relative-URI base propagation, and metadata/payload graph terminal states have focused regression coverage. Same-metaurl multi-file grouping, full `follow-torrent=mem` semantics, session graph restoration, and live protocol interoperability remain open. |
| Integrity and resume | aria2-core/src/checksum/, aria2-core/src/session/ | PARTIAL | Sequential resume detection, defunct-control-file cleanup, existing-file policy, preallocation-safe offset writes, `always-resume`, and `max-resume-failure-tries` multi-URI behavior have focused unit/HTTP E2E evidence. Concurrent control-file lifecycle, piece-level resume semantics, and integrity entry callbacks remain incomplete or unverified. |
| RPC and WebSocket | aria2-rpc/src/ | PARTIAL | JSON-RPC/XML-RPC/WebSocket surfaces, token/Basic authentication, aria2-compatible error/status mapping, feature-specific method/notification discovery, feature-aware `getVersion`, and real HTTP E2E coverage exist. XML-RPC execution faults use HTTP 200 + `faultCode=1`; parser/value failures match the original HTTP 400 empty-body contract. `getServers` is active-only and reports only real in-flight requests; waiting, paused, stopped, or unknown GIDs return execution error code 1. The catalog is 33 core methods plus 2 BitTorrent and 1 Metalink method when enabled; notifications are 5 core plus 1 BitTorrent event when enabled. Task-creation and runtime option values share core validation. Full original-client interoperability, including the browser-extension matrix and complete XML-RPC client coverage, remains unverified. |
| CLI and options | aria2/src/app/, aria2-core/src/config/ | PARTIAL | Core parsing exists; the original short-option contract is now covered for registry mappings, including `-a`/`-p`/`-P`/`-R`/`-u`/`-Z`, and `-h`/`-v`/`-V` help/version/check-integrity actions. The original `file-allocation=prealloc` default is centralized across protocol constructors. Complete option inventory/default/changeability parity, optional-argument/getopt edge cases, version/help text parity, and generated comparison/E2E proof remain open. Rust's extra explicit negation aliases are documented extensions, not an original-parser gap. |
| Public C API/ABI | aria2_original/src/aria2api.cc, src/includes/aria2/aria2.h | PARTIAL | `aria2-core/src/c_api.rs` and `bindings/c/` provide a tested opaque-handle `extern "C"`/cdylib migration interface. It is intentionally source-level and is not binary-compatible with the original C++ classes or STL ABI. |

## Verification Evidence

Verified on 2026-08-09 with single-job builds where needed:

~~~text
cargo fmt --all -- --check                              PASS
cargo check -p aria2-core --all-features --tests -j 1    PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings PASS
cargo test -p aria2-core --test test_e2e_download --all-features -- --test-threads=1 26 passed, 0 failed, 2 ignored
cargo clippy --workspace --all-targets --all-features -- -D warnings PASS
cargo test -p aria2-core --all-features c_api --lib   PASS
cargo test -p aria2-protocol --all-features -j 1        872 passed, 4 ignored
cargo test -p aria2-rpc --all-features --tests -- --test-threads=1 390 passed, 0 failed
cargo test -p aria2 --all-features -j 1                 254 passed, 5 ignored
cargo build -p aria2 --all-features -j 1                PASS
npm run typecheck                                        PASS
npm run build                                            PASS
ARIA2_RUST_BIN=target/debug/aria2c.exe npm test          123 passed
PYTHONPATH=.codex-python-deps python -m pytest ...       136 passed
~~~

Focused HTTP resume regression evidence (2026-08-09):

~~~text
cargo test -p aria2-core --test test_e2e_download resume_failure --all-features -- --test-threads=1  4 passed, 0 failed
cargo test -p aria2-core --lib request::request_group::options::tests::rpc_option_map_uses_aria2_wire_strings --all-features -- --exact  PASS
~~~

Latest RPC compatibility checkpoint (2026-08-09): unknown and
non-changeable options are ignored as in `aria2_original`; a recognized option
with an invalid value returns execution error `code=1` and HTTP 400 for
JSON-RPC. `addUri`, `addTorrent`, and `addMetalink` use the same core parser as
runtime option updates. Only registry-declared cumulative options accept RPC
arrays; ordinary options cannot be silently converted from arrays. The
checkpoint includes 220 library tests, 18 integration tests, 55 all-method
HTTP tests, 40 HTTP/WebSocket/XML-RPC route tests, 5 server-config tests, 3
HTTPS tests, 31 mock-server tests, 8 header/progress tests, and 10 stress
tests in the 390-test command above. The
active/reserved task changeability policy is centralized with the global
policy in `aria2-core/src/config/runtime.rs`; `request_group` only preserves
the historical re-export path. This is a RPC-scope result only; it does not
establish workspace-wide completion or full compatibility with every original
browser client.

The JSON-RPC wire adapter now has direct source-backed regression coverage for
the original envelope rules: `jsonrpc` is ignored, omitted `params` becomes an
empty positional list, missing `id` and object params become object-level
errors, non-object batch elements are skipped, and an empty batch returns
`[]`. This improves wire compatibility but does not prove interoperability
with every browser extension.

The legacy `/jsonrpc` GET/JSONP adapter is also source-backed against
`aria2_original/src/json.cc`: it matches raw `key=value` prefixes, percent-
decodes only `params`, preserves the original permissive Base64 failure path,
keeps `method`/`id`/`jsoncallback` text unnormalized, and returns the original
parse error for a request without a query. JSONP callbacks are emitted using
the original verbatim callback rule for strict wire compatibility; callers
must therefore use the same authentication and deployment protections as the
original endpoint. The new GET/JSONP regression cases pass as part of the
40-route HTTP/WebSocket/XML-RPC target.

The XML-RPC adapter now also has source-backed HTTP contract coverage against
`aria2_original/src/HttpServerBodyCommand.cc`: parser or XML value conversion
failures return HTTP 400 with an empty body and no `Content-Type`, while a
successfully parsed method execution failure returns HTTP 200 with an XML
fault whose `faultCode` is `1`. The all-features RPC suite is 390 passed and
0 failed after this regression was added.

The `aria2.getServers` adapter now follows the original active-only contract:
it emits one file-index entry per active file and includes only requests that
currently have peer statistics. Configured mirrors are not emitted as fake
servers; waiting, paused, stopped, and unknown GIDs map to execution error
code 1 with HTTP 400 for JSON-RPC.

The core command `cargo test -p aria2-core --lib --tests --all-features --
--test-threads=1` completed with exit code 0. Its library target reported
3,278 passed and 1 ignored; every integration and performance target in the
same command also completed with 0 failures. The aggregate workspace command
`cargo test --workspace --all-features -j 1` has not been used as a green gate,
so this document does not claim one workspace aggregate run.

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
- HTTP sequential resume now distinguishes a Range request answered with 200:
  default `always-resume=true` returns `CannotResume`, while
  `always-resume=false` restarts from byte zero. Multi-URI failover and
  `max-resume-failure-tries` are covered for the sequential HTTP path; the
  concurrent and cross-protocol retry matrices remain open.
- CLI/config defaults, changeability, error semantics, optional-argument and
  help/version text parity, and the complete original-client matrix still need
  generated comparison and E2E proof.
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
| RPC option parsing | `aria2-core/src/config/option/registry.rs`, `request/request_group/options.rs`, `config/runtime.rs`, and `request/request_group/options_ops.rs` | Keep original runtime changeability, enum choices, and typed string/size/integer/boolean parsing in core. `OptionRegistry::parse_rpc_value` validates transport values and `DownloadOptions::try_from_rpc_options` validates task creation; RPC handlers only normalize transport values and map parse failures to aria2 execution errors. Do not add another option whitelist or parser in the RPC crate. |
| Request-group identity lookup | `aria2-core/src/request/request_group_man/mod.rs` | Keep `active` and `reserved` as scheduling stores, but use one canonical GID index for all non-terminal groups. Active/reserved movement does not remove the index; terminal demotion/removal does. RPC, C API, session snapshots, and status lookup therefore share one stable identity seam without exposing the internal storage choice. Query snapshots preserve active-first and reserved FIFO order, then append canonical-only groups observed during a transfer window. |
| Resume policy | `aria2-core/src/engine/download_command/execute.rs` and `request/request_group/control_ops.rs` | Keep HTTP response interpretation in `SequentialDownloader`; command-level URI selection, atomic resume-failure accumulation, and fresh-download fallback stay in `DownloadCommand`. Do not make RPC or protocol adapters reinterpret `always-resume` or `max-resume-failure-tries`. |
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
