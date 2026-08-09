# aria2_rust Comprehensive Gap Analysis
# Deep-comparison audit against C++ aria2_original and aria2-next
# Updated: 2026-08-09 (external-contract policy, FTPS status, retry, and CORS reconciliation)

> This file is a historical deep-comparison audit, not the current completion
> gate. The current external-compatibility status is maintained in
> `docs/compatibility-status.md`, which supersedes older summary counts and
> module labels below. In particular, the historical RPC "Complete" statement
> must not be read as browser-extension or full original-client compatibility;
> the current RPC status remains `PARTIAL` until that interoperability matrix
> is reproducibly green.

## Executive Summary

This document consolidates findings from module-by-module deep-comparison audits of the
aria2_rust project against the C++ original (`aria2_original`) and `aria2-next`.

**Overall status:**

| Status | Count | Description |
|--------|-------|-------------|
| Complete | 14 | Module is functionally complete with full test coverage |
| Partial | 6 | Core logic works but significant gaps remain |
| Missing | 2 | Module is absent or only a stub |

**Test counts:**
- `aria2-protocol`: 732 passed (with `--features bittorrent,metalink`)
- `aria2-core`: 3295 passed, 1 ignored
- `aria2-rpc`: 198 passed
- `aria2` (CLI): 32 passed
- **Total: 4257 passed / 0 failed / 1 ignored** (2026-08-01)

**Priority classification:**
- **P0** (9 items): Protocol-breaking / Security-critical -- must fix before any production use
- **P1** (28 items): Feature-breaking -- major functionality missing or wrong
- **P2** (17 items): Minor / Cosmetic -- quality of life improvements

**Key changes since previous audit (2026-08-01 deep-comparison refresh):**

- **HTTP range/fallback safety:** Range responses now validate start/end/entity length; buffered and streaming segment downloads reject short or oversized bodies and invalid 200/206/416 combinations. Segment completion requires exact declared length and is idempotent. Fallback preserves only complete segments, so partial writes from failed requests are re-covered by full sequential gaps.
- **Retry contract:** `aria2_original`'s default `max-tries=5`, total-attempt counting, and `max-tries=0` unlimited behavior are now centralized in Rust `RetryPolicy` and exercised across sequential HTTP, concurrent segments, and FTP. The focused retry evidence does not establish whole-workspace or original-client compatibility.
- **Browser-facing CORS contract:** the RPC HTTP server now emits no CORS headers by default, enables them only through explicit origin configuration or `rpc-allow-origin-all=true`, and returns the original `Access-Control-Max-Age: 1728000` value for opted-in preflight responses. Live OPTIONS E2E coverage protects both default-off and wildcard-on behavior; the complete browser-extension matrix remains open.

**Key changes since previous audit (2026-07-30 deep-comparison refresh):**

- **StreamFilter** upgraded from ~58% to Complete: GZip, BZip2, Deflate, Chunked, NullSink all
  implemented with true streaming/incremental support.
- **FTPS/TLS** is a Rust-only extension, not an `aria2_original` compatibility requirement:
  `tls.rs` implements AUTH TLS, PBSZ 0, PROT P with `tokio_rustls`; plaintext downgrade
  rejection is covered, while positive TLS-server interoperability remains unverified.
- **FTP-over-HTTP-proxy tunneling** upgraded from Missing to Complete: `proxy_tunnel.rs`
  implements CONNECT method tunneling with proxy auth support.
- **FTP negotiation** upgraded from ~52% to Partial: MDTM, PWD/CWD, FEAT, SIZE, REST all
  implemented; full 30-step C++ state machine replaced by async negotiation (architectural
  difference, not a gap).
- **DHT** upgraded from Partial to Near-Complete: Full `DhtEngine` with routing table, bootstrap,
  lookup/announce, token tracker, peer announce storage, task system, message codec,
  serialization, UDP transport.
- **RPC** remains Partial for the migration gate: the wire adapters and focused source-backed
  behavior exist, but full original-client interoperability, including browser extensions and
  complete XML-RPC coverage, is still unverified.
- **HTTP Tail Reclaim** confirmed Complete: `HttpTailReclaimState`, `is_http_tail_blocked()`,
  `should_reclaim_http_tail_segment()`, `updateTailReclaimProgress()`,
  `ConnectionStallTracker` all implemented.
- **Option Registry** now at 164 of 212 C++ options (77% coverage).
- **FileAllocation Iterators** now include `SingleFileAllocationIterator`,
  `FallocFileAllocationIterator`, `TruncFileAllocationIterator`,
  `AdaptiveFileAllocationIterator`, `MultiFileAllocationIterator`.

---

## Priority Classification

- **P0**: Protocol-breaking / Security-critical -- must fix before any production use
- **P1**: Feature-breaking -- major functionality missing or wrong
- **P2**: Minor / Cosmetic -- quality of life improvements

---

## Fixed in This Session

| # | Module | Fix | Details |
|---|--------|-----|---------|
| 1 | Cookie | `to_netscape_line` inverted domain dot | `Cookie::to_netscape_line()` now correctly prepends `.` for non-host-only cookies and omits it for host-only cookies, matching C++ `Cookie::toNsFormat()` |
| 2 | LPD | BEP14 message format | `LpdAnnouncer::announce()` now emits exact `BT-SEARCH * HTTP/1.1\r\nHost: ...\r\nPort: ...\r\nInfohash: ...\r\n\r\n\r\n` format matching C++ `createLpdRequest()` |
| 3 | Checksum | `PieceHashValidator` configurable hash | Hash algorithm now read from `DownloadContext::get_piece_hash_type()` instead of hardcoded SHA-1; supports sha-1, sha-256, sha-512, md5 |
| 4 | Checksum | `WholeFileChecksumValidator` added | New `WholeFileChecksumValidator` with streaming hash state machine (SHA-1/SHA-256/SHA-512/MD5), matching C++ `IteratableChecksumValidator` |
| 5 | Checksum | `IteratableChecksumValidator` added | `ValidatorKind::WholeFile` variant now exists in the enum-dispatch hierarchy |
| 6 | Checksum | `md5` crate migration | All MD5 hashing uses the `md5` crate (was previously inconsistent) |
| 7 | Segment | `UnknownLengthPieceStorage` alignment | All PieceStorage trait methods implemented; `read_data()` via DiskAdaptor; proper single-piece model semantics |
| 8 | Metalink | `select_mirrors_by_priority` ascending sort | Priority sort now uses ascending order (lower number = more preferred), matching C++ `PriorityHigher` comparator. |
| 9 | WebSocket | `rpc-max-request-size` OOM guard | `DEFAULT_RPC_MAX_REQUEST_SIZE` = 2 MiB constant in `aria2-rpc/src/constants.rs`, matching C++ `PREF_RPC_MAX_REQUEST_SIZE` default |
| 10 | WebSocket | Incoming JSON-RPC over WebSocket | `process_ws_jsonrpc()` now processes incoming JSON-RPC requests over WebSocket sessions |
| 11 | LPD | Private torrent LPD filter (BEP 0027) | `LpdManager::register_torrent()` now rejects private torrents |
| 12 | Checksum | `MessageDigest::is_stronger()` | `HashType::strength()` and `HashType::is_stronger()` added |
| 13 | Metalink | Multi-file Metalink support | `Metalink2RequestGroup` conversion creates one `RequestGroup` per file |
| 14 | FileAllocation | FileAllocationMan queue + FileAllocationCommand | FIXED | `file_allocation_man.rs` grew from a data-structure skeleton into a real queue + background worker: chunked cooperative allocation (256 KiB zero-fill chunks with `yield_now`), sequential by default, `oneshot` completion notifications, cancellation on engine halt, disk-space check, resume-safe (skips files already at target length). Wired into HTTP `DownloadCommand`, `BtDownloadCommand` (single + multi-file via `MultiFileLayout`) and magnet (now passes the real options instead of `DownloadOptions::default()`). `--file-allocation` finally applies to BT downloads. 16 tests |
| 15 | **BitTorrent** | **`PiecePicker` was a stub returning `None`** | `select()` / `pick_next()` unconditionally returned `None`, so `BtPieceSelector::select_next_piece` could never choose a piece -- **BT downloads could not transfer any data**. Rewritten as a real picker: 7 `ScanOrder` strategies (Forward / Backward / Rarest / Random / LongestRun / Priority / Geometric), head/tail cursors giving amortised O(1) sequential selection, endgame mode (default threshold 20), `mark_in_progress` / `is_in_progress` / `is_completed` / `set_priority`. `remaining_count()` and `is_complete()` dropped from O(n) to O(1). Randomness from an inline xorshift64\* -- no new dependency. 35 unit tests |
| 16 | **DHT** | **`DhtEngine::start()` blocked on public bootstrap** | `start()` awaited a full public-network bootstrap inline, hanging 6 tests for 60 s+ each and stalling engine startup. Bootstrap now runs as a background task (`spawn_bootstrap()`) wrapped in `tokio::time::timeout` (default 60 s); added `DhtEngineConfig::local()` (`port: 0`, `bootstrap_on_start: false`) for tests. Matches C++ aria2, where bootstrap is an async command inside the event loop and never blocks startup |
| 17 | **bencode** | **Unbounded recursion -> stack-overflow DoS (P0 security)** | The recursive-descent decoder had no nesting limit. A hostile `.torrent` made of a long run of `l`/`d` opener bytes drives unbounded recursion into a stack overflow, which Rust cannot catch -- the process aborts. Now enforces `MAX_NESTING_DEPTH = 50`, matching C++ `BencodeParser::pushState` (`ERR_STRUCTURE_TOO_DEEP`). Also switched the byte-string length prefix to `checked_add`: a `usize::MAX` length wrapped the addition, slipped past the `data_end > bytes.len()` bounds check and panicked inside the slice index. 5 regression tests including a 200 000-level nested input |
| 18 | **Checksum** | **`PieceHashValidator` hardcoded SHA-1** | Item 3 above claimed the algorithm was read from `DownloadContext::get_piece_hash_type()`; re-audit shows `compute_hash()` still hardcoded `sha1::Sha1`. Any download with non-SHA-1 piece hashes (Metalink `<pieces type="sha-256">`, HTTP piece hashes) compared a 40-char digest against a 64-char expected value, so **every piece failed** and triggered endless re-downloads. Now resolves the algorithm from the context via `MessageDigest`, treats an empty type as SHA-1 (BitTorrent), and fails loudly on an unknown algorithm instead of silently falling back. Duplicated cursor-advance logic extracted into `advance()` so no exit path can stall the iteration. 5 regression tests |
| 19 | **Metalink** | **Chunk-level piece hash verification missing** | `verify_pieces()` now checks every `<pieces>` chunk of a downloaded file before write (mirrors C++ `MetalinkEntry::chunkChecksum`). The parser was also broken for pieces: v4 `<hash>` children were mis-parsed as whole-file hashes, v3 concatenated text produced a single bogus entry via `split_whitespace`, and `piece_count()` divided by hex length. Now: v4 `<hash>` children collected per piece, v3 text chunked by `hash_len`, `piece_count()` returns `hashes.len()`. Wired into `FileDownloadInfo` for both single-file and multi-file modes. 3 parser tests + 1 verify test |
| 20 | **CheckIntegrity** | **CheckIntegrityMan queue missing** | The pre-existing `CheckIntegrityKind`/`StreamCheckIntegrity`/`BtCheckIntegrity` validators had **zero callers** — the HTTP and BT download paths write plain files and never build a `PieceStorage`, so the PieceStorage-coupled validators were orphans. Added `check_integrity/man.rs`: `CheckIntegrityMan` (queue + background worker, C++ CheckIntegrityMan + Dispatcher + Command semantics), `CheckIntegrityTask` trait, and `FileChunkValidator` (file-direct chunk hashing, no PieceStorage). Wired into BT single-file and HTTP (when the context carries piece hashes) download commands; `--check-integrity` option plumbed through DownloadOptions/apply/RPC/session. 5 tests |
| 21 | **Metalink** | **Torrent metaurl handling missing (BtDependency)** | `MetalinkDownloadCommand` rejected any file without an HTTP URL (`new` returned "Metalink file has no download URL"); metaurl-only files were dead. Now: constructor accepts files that carry a `mediatype="application/x-bittorrent"` metaurl; `execute()` falls back to downloading the `.torrent` by priority and running `BtDownloadCommand` (C++ `BtDependency`) when no mirror succeeds. `FileDownloadInfo.torrent_metaurls` populated for single + multi-file modes; whole path gated on the `bittorrent` feature. 1 test |
| 22 | **BitTorrent** | **Seed phase never re-announces to tracker** | While seeding, `BtSeedManager` kept the swarm informed of nothing: no periodic announce at all (C++ `SeedCheckCommand` re-announces at the tracker interval so leechers can find the seeder). Added `TrackerAnnouncer` + peer id to `BtSeedManager`, re-announce driven by the announce state machine interval inside `run_seeding_loop`; `run_seeding_phase` builds the announcer from the torrent announce list and now receives the real info hash (was all-zeros). `new_with_announcer` constructor |
| 23 | **BitTorrent** | **BEP 5 DHT Port message never acted on** | The real download loop read Port messages but discarded them (only the orphan `BtPeerInteraction::dispatch_message` path logged them — it has no production callers). Port(port) from a peer now adds `(peer_ip, port)` into the DHT routing table via `add_node`, spawned because the ping waits for a response synchronously (`wait_for_piece_block` / `wait_for_any_piece_block`, `dht_engine` threaded from `download_pieces_loop`). Port(0) and unknown-peer-ip are ignored. Mirrors C++ `BtPortMessage::doReceivedAction()` |
| 24 | **BT/Segment** | **advertisePiece() not wired (P1 #14)** | Audited: HTTP advertises completed segments via `segment_man_ops::complete_segment`; the BT path already broadcasts a HAVE message to every peer on piece completion (`BtPeerInteraction::broadcast_have`, called in `piece_download.rs`), which is the functional equivalent of C++ `advertisePiece()`. Marked DONE — no code change needed |
| 25 | **RPC** | **Option name mismatches silently drop user config** | Registry name `bt-force-encryption` was read by RPC as `bt-force-encrypt` (never matches), and `max-tries` as `max-retries`. Fixed with dual-name lookup (registered name first) in `rpc_options_to_download_options` and in the `changeOption` runtime handler (`options_ops.rs`). Also added `bt-tracker` end-to-end: `DownloadOptions.bt_tracker`, parsed in apply/RPC/session (array or comma/newline separated), and the BT `TrackerAnnouncer` now uses user trackers instead of the torrent announce list (C++ semantics) |
| 26 | **RPC** | **bt-tracker option missing entirely** | See #25 — `--bt-tracker` now overrides the torrent announce list in `peer_management.rs` |
| 27 | **RPC** | **changeGlobalOption never applied to downloads** | `add_task` used only the per-call options map; user-set global options (`aria2.changeGlobalOption`) were stored for `getGlobalOption` but never merged into downloads (C++: global options are session defaults for every subsequent download). Added `user_global_opts` (user-set only — registry-default values seeded at startup must not leak into downloads) written by `changeGlobalOption`, merged (task opts win) in `add_task`. Field-level audit: RPC now reads all 58 `DownloadOptions` fields |
| 28 | **Engine** | **max-concurrent-downloads not consumed by engine** | `RequestGroupMan.max_concurrent` was hardcoded to 5 — neither CLI `-j/--max-concurrent-downloads` nor RPC `changeGlobalOption` took effect. Fixed both paths: CLI startup now applies the option to `RequestGroupMan` before tasks are added; RPC `changeGlobalOption` sends `EngineCommand::SetMaxConcurrent` (engine loop reduces excess active downloads via `reduce_to_limit`, already implemented). 1 test. **Global rate limiter now wired** (see #30): `max-overall-download-limit` / `max-overall-upload-limit` passed through `DownloadEngine.global_limiter` → `ThrottledWriter` dual-bucket serial acquire |
| 29 | **Selector** | **ServerStatMan instances fragmented across downloads** | Every `DownloadCommand::new_with_group` created its own `Arc<ServerStatMan::new()`, so `AdaptiveUriSelector` server speed statistics were never shared across downloads — adaptive mirror selection degraded to per-download heuristics with no cross-download learning. Fixed: `ServerStatMan::shared()` process-level singleton (`OnceLock`, same pattern as `FileAllocationMan::shared()`); all 5 creation sites (2 in `download_command/mod.rs`, 3 in `concurrent_download/pipeline.rs`) now use `ServerStatMan::shared().clone()`. The existing `new_with_stat_man` constructor is now exercised through the shared instance |
| 30 | **Engine** | **Global rate limiter never wired to downloads** | `DownloadEngine.global_limiter` field existed but was never passed to `spawn_download_task` or any download command — `max-overall-download-limit` / `max-overall-upload-limit` were dead options. Fixed: `ThrottledWriter` gains `global_limiter: Option<RateLimiter>` field with `with_global_limiter()` builder; `write()` / `write_at()` acquire from both per-download and global limiters (serial acquire — per-download first, then global). Data flow: `DownloadEngine.global_limiter` → `EngineLoopContext` → `spawn_download_task` → `create_command_for_uri` → `DownloadCommand::set_global_limiter` → `SequentialDownloader` / `ConcurrentDownloader` → `ThrottledWriter`. All download paths fully wired: HTTP sequential + concurrent (6 acquire sites), BT (`BtDownloadCommand`), FTP (`FtpDownloadCommand`), SFTP (`SftpDownloadCommand`), Metalink (`MetalinkDownloadCommand`), Magnet (forwarded). Each command struct gains `global_limiter: Option<RateLimiter>` field + `set_global_limiter()` setter; `ThrottledWriter::with_global_limiter()` applied at all 4 writer creation sites. `RateLimiter` is `Clone` and shares `Arc<Inner>`, so cloning is cheap |
| 31 | **RPC** | **Token comparison timing side-channel** | `server.rs` token verification used plaintext `==` comparison, vulnerable to timing attacks. Fixed with `constant_time_eq` at all 3 token comparison sites. Matches C++ security semantics |
| 32 | **Session** | **GID not zero-padded in session serialization** | Session file wrote GIDs as bare hex (`{:x}`) without zero-padding to 16 chars. C++ aria2 expects `{:016x}` format and cannot load sessions with short GIDs. Fixed in `session_serialize_impl.rs` |
| 33 | **Session** | **Restore discarded persisted GIDs** | Session restore now inserts each entry through `RequestGroupMan::add_group_with_gid(GroupId::new(entry.gid), ...)`, rejects duplicate GIDs, advances the automatic allocator beyond restored IDs, and restores persisted progress counters. App and manager regressions verify GID lookup and duplicate handling. Metadata-generated child groups with `belongs_to_gid` are excluded from both standard session serialization and JSON `.aria2` resume persistence, matching aria2_original's `writeDownloadResult()` filtering; persisted GIDs are parsed strictly instead of silently replaced with random IDs, and duplicate IDs are skipped during JSON restore. A manager-bound `load_state_into_manager()` path now preserves restored group objects, validates conflicts through `add_restored_group()`, and advances the automatic allocator; the legacy Vec-based loader remains for compatibility. Parent/child runtime linkage and Metalink dependency reconstruction remain separate gaps. |
| 34 | **Engine** | **Request pause/unpause/remove scheduling broken** | `process_task_completions` in `engine_loop.rs` did not distinguish pause from error/completion, causing paused tasks to be demoted as if they had finished. `requeue_non_terminal_groups` in `request_group_man/demotion.rs` now re-queues groups in non-terminal states (Paused) back to active instead of dropping them. `mark_session_dirty` hooked into engine loop for session auto-save on state changes |
| 35 | **Content-Disposition** | **Trailing `;` rejected (C++ bug #1118)** | C++ aria2's terminal state switch rejects `CD_BEFORE_DISPOSITION_PARM_NAME`, causing it to reject headers ending in `;` — a known bug (GitHub issue #1118, open 5+ years) that breaks downloads from S3/CloudFront/nginx which routinely emit trailing `;`. Rust parser now accepts `ParseState::BeforeParmName` as a valid terminal state, matching RFC 6266's `*( ";" disposition-parm )` grammar (zero or more trailing empty parameters). Empty parameters in the *middle* (`attachment; ;filename=foo`) are still rejected by the `BeforeParmName` state handler. 7 trailing-`;` tests, 110 total tests |
| 36 | **SFTP** | **`FileOpError` type missing — sftp feature broken** | `types.rs` imported `FileOpError` from `aria2_protocol::sftp::file_ops`, but that module never defined it — all methods returned `Result<_, String>`. This made `cargo build --features sftp` fail (E0432). Fixed: `FileOpError` enum added to `file_ops.rs` with `NotFound`/`PermissionDenied`/`Network`/`Other` variants + `From<String>` impl that parses SFTP status codes (SSH_FX_NO_SUCH_FILE=2, SSH_FX_PERMISSION_DENIED=3, SSH_FX_NO_CONNECTION=6, SSH_FX_CONNECTION_LOST=7) + `Display` impl. 3 call sites in `execution.rs` convert `String` → `FileOpError` before passing to `map_file_op_error` |
| 37 | **Retry** | **`max-tries` semantics diverged across download paths** | `aria2_original` defaults to 5 total attempts and treats `0` as unlimited. Rust previously mixed retry-count and attempt-count interpretations and used a lower default. Fixed by making `RetryPolicy` the shared typed seam for sequential HTTP, concurrent segments, and FTP; added policy, executor, HTTP E2E, and FTP E2E coverage. |
| 38 | **RPC/HTTP** | **CORS preflight cache lifetime differed from aria2_original** | The Rust server returned `Access-Control-Max-Age: 86400`; `aria2_original/src/HttpServerBodyCommand.cc` returns `1728000`. Fixed the shared `CORS_MAX_AGE` wire constant and added a live OPTIONS E2E assertion. |
| 39 | **RPC/HTTP** | **CORS was enabled by default in Rust** | `aria2_original` only sets `Access-Control-Allow-Origin` when `rpc-allow-origin-all=true`; Rust `CorsConfig::default()` previously meant wildcard access. Fixed by making the default empty/disabled, adding an explicit `allow_all_origins()` constructor, and covering both branches at the live HTTP seam. |

---

## Module-by-Module Analysis

### 1. Download Engine

**C++ files:** `DownloadEngine.h/.cc`, `DownloadEngineFactory.h/.cc`, `AbstractCommand.h/.cc`, `Command.h/.cc`
**Rust files:** `aria2-core/src/engine/download_engine/` (4 files), `aria2-core/src/engine/engine_loop.rs`

| C++ Feature | Rust Status | Notes |
|-------------|-------------|-------|
| `DownloadEngine::run()` | Complete | v1 (JoinSet-based) + v2 (RequestGroupMan-based) engine loops |
| `SocketPool` (multimap) | Architectural difference | Rust uses `reqwest` connection pool instead of manual socket pooling; `FtpConnectionPool` for FTP |
| `CookieStorage` | Present | Owned by engine in C++; separate `CookieStorage` in Rust HTTP module |
| `BtRegistry` | Complete | `Arc<RwLock<BtRegistry>>` owned by engine, accessible via `bt_registry()` |
| `DNSCache` | Fixed | Engine-owned `Arc<Mutex<DnsCache>>`; positive/negative entries are keyed by `(hostname, port)`, candidates retain Good/Bad state, FTP and HTTP command creation resolve through the shared cache, and HTTP clients apply cached addresses with `resolve_to_addrs`. Exhausted-set refresh remains explicit via cache invalidation. |
| `AuthConfigFactory` | Fixed | Centralized Rust `AuthConfigFactory` now covers HTTP/HTTPS, FTP/SFTP, URL credentials, activated BasicCred cache, Netrc/CLI precedence, HTTP `default`-entry exclusion, FTP anonymous defaults, and challenge activation. |
| `CUIDCounter` | Architectural difference | Rust uses `GroupId` (auto-incrementing atomic) instead of C++'s `cuid_t` |
| `CheckIntegrityMan` | Partial | Rust has a process-wide sequential async queue, chunked worker, cancellation, completion channels, live atomic current/total progress, and HTTP whole-file verification now marks `DownloadContext` as verified atomically before completion results are emitted. HTTP/stream download preflight routes failed validation back into a clean download path instead of terminating the request; remaining difference is full engine-level dispatcher/post-validation command wiring for every protocol. |
| `StatCalc` | Partial | C++ has `ConsoleStatCalc` for terminal stats. Rust has `PerformanceMonitor` + `AtomicMetrics` |
| `EventPoll` | N/A (architectural) | Rust uses tokio runtime instead of `select()`/`epoll`/`kqueue` |
| `WebSocketSessionMan` | N/A | Handled by axum in RPC crate |
| `RequestGroupMan` | Complete | `Arc<RwLock<RequestGroupMan>>` with DashMap active + VecDeque reserved + Vec stopped |
| `FileAllocationMan` | Complete | Async queue/worker, sequential dispatch, cancellation, disk-space checks, resume-safe allocation and live current/total progress are implemented; this is the Rust async equivalent of C++ `FileAllocationMan` plus `FileAllocationCommand`. |
| `poolSocket()`/`popPooledSocket()` | Partial | Raw HTTP manager has keyed LRU pooling, timeout cleanup, `discard`, and context-based idle peer eviction; reqwest download paths still use the library-managed pool |
| `validateToken()` | Fixed | `RpcAuthMiddleware` now creates one random HMAC-SHA256 key per middleware, caches the expected digest of `rpc-secret`, and compares token digests in constant time, matching C++ `DownloadEngine::validateToken()`. |

### 2. BitTorrent Core

**C++ files:** ~53 header files (Bt*.h + DefaultBt*.h)
**Rust files:** 80+ bt_*.rs files in `aria2-core/src/engine/`

#### Completed

| Component | Status | Tests | Notes |
|-----------|--------|-------|-------|
| DefaultBtRequestFactory | Complete | 34 | Target piece list, normal/endgame/choke modes |
| DefaultBtMessageReceiver | Complete | 16 | NAT-checking quick-reply, handshake flow |
| Message Type Side Effects (all 16) | Complete | -- | All `doReceivedAction` equivalents in `BtPeerMessageHandler` |
| Message Validators | Complete | 30 | BtMessageValidator with index/range/bitfield/handshake |
| BtRegistry | Complete | 3+ | Info-hash lookup, BtPeerBlocklist, Arc<> references |
| BtRuntime | Complete | -- | All C++ methods; saturating arithmetic improvement |
| BtProgressInfoFile | Complete | -- | Binary format compatible with C++ (v0/v1) |
| BtPeerBlocklist | Complete | -- | Fully integrated into BtRegistry + DefaultPeerStorage |
| PieceStorage Interface | Nearly Complete | 18+ | 30+ trait methods, stream selectors, filter-aware selection |
| DefaultBtInteractive | Complete | 92 | 12-step loop, checkHave optimization, PEX pending flag |
| Extension Messages | Complete | 32+ | ExtensionHandshake, UtMetadata (BEP 9), UtPex (BEP 11) |
| BtAnnounce | Complete | -- | All C++ methods + health tracking + exponential backoff |
| BtMessageDispatcher | Complete | -- | All C++ methods + anti-flooding + idle detection |
| BtChokeManager | Complete | -- | Optimistic unchoke, seed/leecher state, round-robin |
| DefaultPeerStorage | Complete | -- | Peer entries, onwalk, good/bad peer classification |
| BtWebSeed | Complete | -- | Web seed client + manager + URL parser |
| BtSeedManager | Partial | -- | Seed criteria tracking; re-announce communication missing |
| BtPieceDownloader | Partial | -- | Piece download pipeline; Fast Extension Reject not fully integrated |
| BtUploadSession | Complete | -- | Upload slot management |
| MSE Handshake | Complete | `aria2-protocol` | Standard 768-bit DH + RC4 implementation is the only BT MSE implementation; obsolete non-interoperable engine implementation removed |
| BtDownloadCommand | Complete | 92+ | Full execute sub-modules: piece_download, peer_management, web_seed, pex, dht, bep6, finalization |
| Metadata Exchange | Complete | -- | `metadata_exchange.rs` for magnet link metadata fetch |
| Magnet Download | Complete | -- | `magnet_download_command.rs` |

#### Remaining Gaps

| Component | Priority | Gap |
|-----------|----------|-----|
| BtSetup | PARTIAL | Async `BtSetup::setup()` now validates BT context and records metadata/private-torrent mode, while `BtDownloadCommand` owns tracker, peer, choke and discovery execution; engine-level listener/command scheduling and full C++ DHT/LPD command graph are not yet unified behind one setup context. |
| DefaultBtMessageFactory | PARTIAL | `BtPeerInteractive` now owns a factory-equivalent domain validator and the primary receive loop validates messages at the connection boundary. Seeder upload sessions also configure and apply the same domain validator before serving requests. BEP 5 now exposes an injectable DHT port handler on `BtPeerInteractive`. Extension dispatch now also exposes an injectable update sink, allowing metadata/PEX consumers to be wired without coupling the interaction state machine to storage or DHT implementations; PeerStorage, PieceStorage and concrete extension-factory context injection remains distributed across specialized dispatch paths. |
| Zero-copy Piece optimization | P2 | Currently copies Piece data; C++ uses zero-copy path |
| `addAllowedFastMessageToQueue()` | DONE | Canonical BEP 6 `compute_fast_set` is now used by the production BT setup path; identity-keyed sent tracking prevents duplicate AllowedFast messages. |
| Write Disk Cache (WrDiskCache) | PARTIAL | `CachedDiskWriter` is used by BT single-file random piece writes and flushes a snapshot through the positioned writer, marking entries clean only after writer success; concurrent replacement is protected by sequence checks. Completed BT and web-seed pieces now enter the writer through `Bytes` without an extra Vec-to-cache copy, and multi-file boundaries use zero-copy `Bytes::slice`. Remaining difference is C++ piece/segment-scoped cache aggregation and error propagation into piece state. |
| `createFastIndexBitfield()` | P2 | Proper fast-piece filtering |
| Seed phase tracker communication | PARTIAL | `BtSeedManager` now emits `completed` on entry, interval-aware seeding announces, and `stopped` on exit with `downloaded=total`, `left=0`, cumulative `uploaded`, and the stable peer ID; seed criteria set a halt flag propagated to the request group. Full DownloadEngine command rescheduling and tracker event delivery under process-level shutdown still require integration testing. |

### 3. HTTP/FTP

**C++ files:** ~30 Http*.h/.cc + ~15 Ftp*.h/.cc files
**Rust files:** 70+ files in `aria2-core/src/http/` + 25+ files in `aria2-core/src/ftp/`

#### HTTP Module

| Sub-module | Status | Key Gaps |
|------------|--------|----------|
| HttpConnection | Architectural difference | No `HttpRequestEntry` queue (reqwest handles); no `eraseConfidentialInfo()` |
| HttpDownloadCommand | Complete | Tail reclaim, stream filters, cookie management, proxy, checksum all working |
| StreamFilter | Complete | GZipDecoder, BZip2Decoder, DeflateDecoder, ChunkedDecoder, NullSinkFilter with streaming |
| AuthConfig/Auth | Partial | Has Basic, Digest, Netrc auth; missing centralized `AuthConfigFactory` |
| CookieStorage | Partial | Rust now uses domain buckets plus a monotonic LRU tracker, SQLite/Netscape loading, per-domain/global eviction, and RFC matching; storage is process-shared across HTTP commands. Remaining difference is the C++ label tree and engine-level cookie lifecycle integration. |
| Content-Disposition | Complete | RFC 6266 state-machine parser; accepts trailing `;` (diverges from C++ bug #1118); 110 tests |
| Conditional GET | Complete | `conditional_get.rs` with If-Modified-Since / ETag |
| SOCKS Connector | Complete | SOCKS4, SOCKS5, no-proxy matcher |
| Happy Eyeballs | Complete | RFC 8305 dual-stack racing |
| HTTP Proxy | Complete | Forward, tunnel, auth, I/O |
| Splice/Zero-copy | Complete | Linux `splice(2)` |
| Tail Reclaim | Complete | Policy + per-connection stall tracking |

#### FTP Module

| Sub-module | Status | Key Gaps |
|------------|--------|----------|
| FtpConnection | Complete | Commands, connector, FEAT, parser, transfer; production passive data channels now pin to the control peer as in `aria2_original`, while PASV byte fields remain strictly validated |
| FtpNegotiation | Complete | MDTM, PWD/CWD, FEAT, SIZE, REST supported; parsed SIZE values are validated and existing RequestGroup lengths reject mismatches |
| FTPS/TLS | Extension / unverified | Rust implements AUTH TLS, PBSZ 0, PROT P with `tokio_rustls`; plaintext downgrade rejection is covered, but positive TLS-server interoperability remains unverified |
| FTP-over-HTTP-proxy | Complete | CONNECT method tunneling with 407 proxy auth |
| FTP Connection Pool | Complete | Operations, stats, max connections |
| FtpFinishDownload | Complete | 226 response + connection pooling |
| FtpDownloadCommand | Partial | FTP reconciles SIZE with local length, short-circuits an already complete target, truncates oversized targets, queues allocation before REST/RETR, preserves resume progress, and explicitly truncates on REST fallback; full FTP allocation/resume integration tests and nuanced C++ command-chain lifecycle remain. |
| Active mode (PORT/EPRT) | Partial | `FtpDownloadCommand` now reads `ftp-pasv`, advertises EPRT with IPv4 PORT fallback, waits for the server-side data connection after RETR, and preserves passive mode separately. Proxy/NAT external-address configuration and full active-mode integration tests remain. |

#### C++ FTP Command Chain Status

| C++ Module | Rust Equivalent | Status |
|------------|----------------|--------|
| `FtpInitiateConnectionCommand` | `ftp_download_command/execution.rs` | Complete (integrated into async flow) |
| `FtpFinishDownloadCommand` | `ftp/connection/ftp_finish.rs` | Complete |
| `FtpTunnelRequestCommand` + `FtpTunnelResponseCommand` | `ftp/connection/proxy_tunnel.rs` | Complete (collapsed into single async fn) |
| `FtpNegotiationConnectChain` | `ftp/connection/negotiation/` | Complete (replaced by async negotiation) |
| `FtpTunnelRequestConnectChain` | `ftp/connection/proxy_tunnel.rs` | Complete |

### 4. Metalink

**Status:** Partial -- single-file Metalink v3/v4 works; multi-file supported; several features missing
**Rust files:** `metalink_download_command/`, `metalink_post_download_handler.rs`, `metalink_to_request_group.rs`, `aria2-protocol/src/metalink/`

#### Critical Issues (all FIXED)

| # | Issue | Status | Details |
|---|-------|--------|---------|
| 1 | Priority direction reversed | FIXED | `select_mirrors_by_priority()` now sorts ascending |
| 2 | PostDownloadHandler missing | FIXED | `MetalinkPostDownloadHandler` exists |
| 3 | Multi-file Metalink rejected | FIXED | `Metalink2RequestGroup` creates one `RequestGroup` per file |

#### Feature Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 4 | Chunk-level piece hash missing | P1 | FIXED | `verify_pieces()` in `metalink_download_command/execution.rs` checks every `<pieces>` chunk after download; parser fixed for v4 `<hash>` children + v3 concatenated text |
| 5 | Version/language/OS filtering missing | P1 | FIXED | `DownloadOptions` now carries `metalink-version`, `metalink-language`, and `metalink-os`; `MetalinkToRequestGroup::generate_from_bytes()` injects them into the C++-equivalent `query_entries()` filter. |
| 6 | Metalink v3 `verification` element | FIXED | Parser consumes v3 `<verification><hash>` and `<pieces>`; the production command verifies the strongest whole-file hash and piece hashes before accepting a mirror. Remaining difference is streamed/RequestGroup-wide post-download verification. |
| 7 | Torrent metaurl handling | P1 | PARTIAL | metaurl-only files are preserved by `MetalinkToRequestGroup`; the command persists torrent metadata, records `MetadataInfo`, injects the parsed context into its externally-owned group, and routes resolved `bt://` payloads to `BtDownloadCommand`. This is still a command-local fallback, not the independent metadata RequestGroup plus manager-owned `BtDependency` graph required by aria2_original. A reusable `MetalinkRequestGraph` now constructs those two groups and `MetalinkToRequestGroup::create_torrent_graph()` exposes the graph builder. The RPC `aria2.addMetalink` torrent-metaurl path now inserts both groups into `RequestGroupMan` with metadata-first ordering and returns only the payload GID, matching the public RPC contract. Graph insertion is serialized and queue insertion holds one lock. The metadata-file completion to payload-promotion path is covered by an integration-level manager test. CLI HTTP/FTP/Magnet inputs now use typed `EngineCommand::AddDownload` groups and select the v2 manager loop when the batch contains only those supported inputs. Torrent-file inputs now also use v2: the torrent bytes are retained in the group, the URI is normalized to an internal `bt://` form, and `task_spawner` passes the bytes to `BtDownloadCommand::new_with_group`. SFTP CLI inputs now use manager-owned `RequestGroup`s and the v2 task spawner; Metalink-file inputs use the manager-owned resource/graph paths. Mixed batches containing unsupported legacy-only inputs still remain on v1. CLI Metalink files now build manager-owned v2 groups: direct resources become per-file `RequestGroup`s with Metalink output-name overrides, while torrent metaurls become metadata/payload graphs submitted through `EngineCommand::AddMetalinkGraph`; payload/resource GIDs are returned to the CLI caller. Mixed direct-resource + torrent-metaurl entries are now represented by one manager-owned `RequestGroup` carrying the raw Metalink source and selected file index. The v2 task spawner dispatches that group to `MetalinkDownloadCommand`, preserving C++-style direct-mirror-first and torrent fallback semantics without duplicate groups. The independent metadata-first graph remains available for torrent-only entries; mixed direct-first groups now persist and restore their raw Metalink source/file index in session data (base64, serde-default compatible). Post-download verification remains partially RequestGroup-wide. SFTP `ssh-host-key-md` is now mapped through CLI/RPC/options/session into russh-compatible MD5, SHA-1, SHA-256, and SHA-512 fingerprint verification with mismatch rejection. |
| 8 | Location preference not configurable | FIXED | `--metalink-location` is parsed into `DownloadOptions`, injected into `MetalinkToRequestGroup`, split on commas, normalized to lowercase, and applied through `set_location_priority()`; this matches C++ `PREF_METALINK_LOCATION`. |

### 5. FileAllocation

**Status:** Complete -- allocation strategies, iterators, queue manager and worker all present; wired into HTTP/BT/magnet download paths
**Rust files:** `filesystem/file_allocation/` (5 files), `file_allocation_iterator.rs` (544 lines), `multi_file_allocation_iterator.rs` (411 lines)

#### What Works
- `AllocationStrategy` enum: None, Prealloc, Falloc, Trunc, Mmap
- Cross-platform `allocate_file()` with `posix_fallocate`, `ftruncate`, Windows `SetEndOfFile`
- `preallocate_file()` convenience function with progress callback
- Secure-falloc zero-fill on macOS/Windows
- `FileAllocationIterator` async trait for chunked allocation
- `SingleFileAllocationIterator` — zero-fill in 256 KiB chunks
- `FallocFileAllocationIterator` — one-shot `fallocate`
- `TruncFileAllocationIterator` — `ftruncate`/`SetEndOfFile`
- `AdaptiveFileAllocationIterator` — try `fallocate`, fall back to zero-fill
- `MultiFileAllocationIterator` — per-file allocation for torrent multi-file downloads

#### Critical Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 1 | FileAllocationMan queue missing | P0 | FIXED | `filesystem/file_allocation_man.rs` now has a real queue + background worker (chunked, cooperative allocation with `yield_now` between chunks), sequential by default (`max_concurrent=1`), completion notifications via `oneshot`, cancellation on engine halt. Dead `EngineCommand::FileAllocation{Request,Completed,Failed}` variants and their handlers were removed. HTTP `DownloadCommand` and BT `BtDownloadCommand`/magnet all enqueue through the process-wide `shared()` manager; `--file-allocation` now actually applies to BitTorrent downloads (previously BT files grew on demand) |
| 2 | FileAllocationCommand loop missing | P0 | FIXED | Replaced by the worker loop inside `FileAllocationMan` (the async equivalent of C++'s per-tick `allocateChunk()`). `Falloc`/`Trunc` are atomic syscalls; only `Prealloc` zero-fill is chunked (256 KiB, matching C++ `SingleFileAllocationIterator`). The `secure` flag now reaches the fallocate path (the old `FallocFileAllocationIterator` hardcoded `secure=false`) |

#### Feature Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 3 | `BtFileAllocationEntry` missing | P1 | FIXED | BT single/multi-file allocation is queued before peer setup; existing files are skipped safely and allocation completion gates the download path. |
| 4 | `HttpFileAllocationEntry` missing | P1 | FIXED | HTTP and BT commands use the shared async allocation manager before creating their download writers. |
| 5 | Allocation progress events | P2 | FIXED | `FileAllocationMan::current_progress()` now reads live atomically shared iterator progress, matching C++ current/total progress semantics. |

### 6. LPD (Local Peer Discovery)

**Status:** Partial -- BEP14 format fixed; receive loop fixed; one feature missing
**Rust files:** `engine/lpd_manager/` + `lpd_receive_loop.rs`

#### Fixed
- BEP14 message format matches C++ exactly
- Private torrent LPD suppression (BEP 0027)
- LPD receive loop runs as background tokio task
- Duplicate announcement suppression via HashSet

#### Remaining Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 1 | Multicast interface config | P2 | Fixed | `LpdManager::with_interval_and_interface()` and `LpdAnnouncer::with_interface()` select the requested IPv4 interface for multicast membership; default construction preserves all-interface behavior. |

### 7. RPC

**Status:** Method surface complete (36/36); external compatibility remains
`PARTIAL` until the browser-extension, original-client, and full XML-RPC
interoperability matrix is reproducibly green.
**Rust files:** `aria2-rpc/src/` (16 files)

#### RPC Method Coverage (36/36)

| C++ Method | Rust Handler | Status |
|------------|-------------|--------|
| AddUriRpcMethod | handle_add_uri() | Complete |
| RemoveRpcMethod | handle_remove() | Complete |
| ForceRemoveRpcMethod | handle_force_remove() | Complete |
| PauseRpcMethod | handle_pause() | Complete |
| ForcePauseRpcMethod | handle_force_pause() | Complete |
| PauseAllRpcMethod | handle_pause_all() | Complete |
| ForcePauseAllRpcMethod | handle_force_pause_all() | Complete |
| UnpauseRpcMethod | handle_unpause() | Complete |
| UnpauseAllRpcMethod | handle_unpause_all() | Complete |
| AddTorrentRpcMethod | handle_add_torrent() | Complete |
| AddMetalinkRpcMethod | handle_add_metalink() | Complete |
| PurgeDownloadResultRpcMethod | handle_purge_download_result() | Complete |
| RemoveDownloadResultRpcMethod | handle_remove_download_result() | Complete |
| GetUrisRpcMethod | handle_get_uris() | Complete |
| GetFilesRpcMethod | handle_get_files() | Complete |
| GetPeersRpcMethod | handle_get_peers() | Complete |
| GetServersRpcMethod | handle_get_servers() | Complete |
| TellStatusRpcMethod | handle_tell_status() | Complete |
| TellActiveRpcMethod | handle_tell_active() | Complete |
| TellWaitingRpcMethod | handle_tell_waiting() | Complete |
| TellStoppedRpcMethod | handle_tell_stopped() | Complete |
| ChangeOptionRpcMethod | handle_change_option() | Complete |
| ChangeGlobalOptionRpcMethod | handle_change_global_option() | Complete |
| GetVersionRpcMethod | handle_get_version() | Complete |
| GetOptionRpcMethod | handle_get_option() | Complete |
| GetGlobalOptionRpcMethod | handle_get_global_option() | Complete |
| ChangePositionRpcMethod | handle_change_position() | Complete |
| ChangeUriRpcMethod | handle_change_uri() | Complete |
| GetSessionInfoRpcMethod | handle_get_session_info() | Complete |
| ShutdownRpcMethod | handle_shutdown() | Complete |
| GetGlobalStatRpcMethod | handle_global_stat() | Complete |
| ForceShutdownRpcMethod | handle_force_shutdown() | Complete |
| SaveSessionRpcMethod | handle_save_session() | Complete |
| SystemMulticallRpcMethod | handle_multicall() | Complete |
| SystemListMethodsRpcMethod | handle_list_methods() | Complete |
| SystemListNotificationsRpcMethod | handle_list_notifications() | Complete |

#### Remaining Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 1 | Notification format differs from C++ | P1 | FIXED | `DownloadEvent` serializes the exact C++ shape: `jsonrpc`, event method, and `params: [{"gid": "..."}]`. |
| 2 | Extra `onBtCacheChanged` event type | P2 | FIXED | Production `EventType` exposes exactly the six C++ events; non-standard cache/error/resume variants are excluded from `system.listNotifications` and event dispatch. |
| 3 | WebSocket close handling | P2 | FIXED | The Axum session now distinguishes received close frames from EOF, logs the close state, echoes the close frame, and terminates the session cleanly. |
| 4 | Original-client/browser-extension interoperability | P1 | OPEN | JSON-RPC, XML-RPC, WebSocket, GET/JSONP, authentication, error mapping, and method-level behavior have focused source-backed tests, but live interoperability with the complete original client matrix is not yet verified. |

### 8. Cookie / HTTP Auth

**Status:** Partial -- cookie load/save/find works; DomainNode tree missing
**Rust files:** `http/cookie/` (9 files), `http/auth/` (4 files), `config/netrc.rs`

#### Remaining Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 1 | CookieStorage domain tree missing | P1 | PARTIAL | Rust replaced the flat Vec with O(1) domain buckets and a monotonic LRU tracker; it intentionally uses HashMap rather than reproducing pointer-linked label nodes. |
| 2 | LRU tracker missing | P1 | FIXED | `CookieStorage` maintains a monotonic `(sequence, domain)` BTreeSet and evicts least-recently-used domains at the C++ trigger/rate thresholds. |
| 3 | Per-domain cookie limit | P2 | FIXED | Rust enforces `MAX_COOKIE_PER_DOMAIN` (50), removes expired entries first, then replaces the least-recently-accessed cookie. |
| 4 | SQLite cookie parser | P2 | FIXED | `http/sqlite_cookie_parser.rs` implements `Sqlite3CookieParser` with Firefox `moz_cookies` / Chromium `Cookies` schemas (rusqlite bundled); `CookieStorage::load_file` routes by `SQLite format 3` magic. 14 tests |
| 5 | Duplicate Cookie/JarCookie structures | P2 | PARTIAL | `JarCookie` now uses RFC host/domain/path boundary semantics, secure-scheme enforcement, and update identity (name + domain + path); the process-shared `CookieStorage` remains the production owner while `CookieJar` is retained as a persistence/API adapter. |
| 6 | `eraseConfidentialInfo()` | P2 | FIXED | `erase_confidential_info()` masks Authorization, Proxy-Authorization, Cookie, and Set-Cookie headers with the same case-insensitive field-prefix semantics as C++. HTTP request/response logging uses this sanitizer. |

### 9. Checksum / Integrity

**Status:** Nearly Complete
**Rust files:** `checksum/` (7 files)

#### Remaining Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 1 | `Adler32` streaming | P2 | FIXED | `MessageDigest` supports incremental Adler32 updates/finalization; `ChecksumValidator` provides streaming verification with case-insensitive digest comparison. |

### 10. Segment / Piece Storage

**Status:** Nearly Complete
**Rust files:** `segment/` (multiple files), `piece_selector.rs` (576 lines)

#### Remaining Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 1 | `SegmentMan::advertisePiece()` not wired | P1 | FIXED | `SegmentMan::advertise_piece()` delegates to `PieceStorage::advertise_piece()` and `complete_segment()` invokes it on completion. |
| 2 | `SegmentMan::getSegment(FileEntry)` missing | P1 | FIXED | `get_segments_for_file_entry()` builds the C++-equivalent range filter, checks write positions, and cancels out-of-range temporary segments. |
| 3 | `initStorage()` auto-initialization | P1 | N/A | SegmentMan unused by download paths (orphan); see P1 #16 |
| 4 | `setupFileFilter()` / `clearFileFilter()` | P1 | Partial | Placeholder exists; real file entry iteration by engine |
| 5 | `createFastIndexBitfield()` | P2 | FIXED | `DefaultPieceStorage::get_missing_fast_pieces()` builds the equivalent temporary eligible bitfield by intersecting peer availability, local missing/unused state, and AllowedFast indexes. |

### 11. DHT

**Status:** Near-Complete -- full engine implemented; BT integration wiring needed
**Rust files:** `aria2-core/src/dht/` (29 files)
**C++ files:** ~60 DHT*.h/.cc files

| C++ Component | Rust Equivalent | Status |
|---------------|----------------|--------|
| DHTConstants.h | constants.rs | Complete |
| DHTNode.h/.cc | node.rs + node_id.rs | Complete |
| DHTBucket.h/.cc | bucket.rs | Complete |
| DHTBucketTree.h/.cc | bucket_tree.rs | Complete |
| DHTRoutingTable.h/.cc | routing_table.rs | Complete |
| DHTTokenTracker.h/.cc | token_tracker.rs | Complete |
| DHTConnectionImpl.h/.cc | transport.rs | Complete |
| DHTMessageTracker.h/.cc | tracker.rs | Complete |
| DHTPeerAnnounceStorage.h/.cc | peer_announce.rs | Complete |
| DHTMessageDispatcherImpl.h/.cc | dispatcher.rs | Complete |
| DHTMessageReceiver.h/.cc | receiver.rs | Complete |
| All DHT message types | message.rs + message_codec.rs + message_decode.rs | Complete |
| DHTRoutingTableSerializer/Deserializer | routing_table_ser.rs | Complete |
| DHTTask hierarchy | task/ (6 files) | Complete |
| DHTSetup + DHTInteractionCommand | DhtEngine::new() + run() | Complete |
| All periodic commands | Periodic in run() loop | Complete |
| DHTRegistry.h | Owned by DhtEngine | Complete |
| DHTGetPeersCommand | DhtEngine::lookup_peers() | Complete |

#### Remaining Gaps

| # | Issue | Priority | Status | Details |
|---|-------|----------|--------|---------|
| 1 | DHT announce/lookup not wired into BtDownloadCommand | P1 | DONE | `peer_management.rs` `discover_peers` calls `engine.find_peers()` — see also P1 #17 |
| 2 | DHT entry point DNS resolution | P2 | FIXED | Rust bootstrap asynchronously resolves each host with Tokio, filters results to the configured IPv4/IPv6 family, pings every resolved address, and safely skips failed/no-family matches. Remaining difference is the C++ event-poll resolver manager rather than behavior. |
| 3 | DHT MessageFactory dynamic dispatch | P2 | N/A | Rust uses direct function calls -- simpler but less extensible |

### 12. Option / Config

**Status:** Partial -- 164 of 212 C++ option handlers implemented (77%)
**Rust files:** `config/option/` (4 files), `config/option_definitions/` (6+2 files), `config/mod.rs` (528 lines)

| Category | C++ Count | Rust Count | Coverage |
|----------|-----------|------------|----------|
| General | ~60 | 85 | ~140% |
| HTTP/FTP | ~50 | 52 | ~104% |
| BitTorrent | ~60 | 61 | ~102% |
| RPC | ~15 | 15 | 100% |
| Advanced | ~27 | 19 | ~70% |
| **Total** | **212** | **164** | **77%** |

Missing options include: various SFTP options, some advanced logging options, a few edge-case HTTP options, and ED2K-specific options (aria2-next only).

### 13. MSE (Protocol Encryption)

**Status:** Partial
**Rust files:** `aria2-protocol/src/bittorrent/extension/mse_handshake`
**Notes:** Standard MSE/DH/RC4 is used by production, deep integration tests, and benchmarks. Dynamic PEX/tracker peers reuse the initial crypto and session initialization path. Transport-handshake peer IDs are synchronized into `BtPeerConn`/`PeerStats` before registration. Local peer ID generation is now scoped to `BtDownloadCommand` and reused for tracker started/periodic/completed/stopped announcements and self-connection checks. Dynamic PEX/tracker connections now initialize snub timing, AllowedFast/suggest state, peer tracker bitfields, and PEX registration through the same download-loop path. Have/Bitfield/HaveAll/HaveNone now emit exact old/new bitfield transitions and update global piece availability statistics through the PieceProvider boundary; regression coverage verifies Have, HaveAll, and HaveNone transitions. Block failures now return concrete failed peer addresses; the download loop releases session resources, removes failed peers, synchronizes choking and endgame index state, removes peer bitfield statistics, rebuilds index-based PEX/snubbing/AllowedFast state, and propagates final writer flush/close errors. Choking/bitfield-tracker removal and external-client interoperability still require verification. Endgame duplicate-request ownership, PEX-enabled tracking, snub timing, AllowedFast bookkeeping, and Suggest counters now use stable `PeerKey` identities rather than compacted vector positions. Choking algorithm state now stores explicit snubbing and optimistic-unchoke ownership by `PeerIdentity` (peer ID plus address). Identity-aware remove, data-received, best-peer selection, optimistic-unchoke, and choke-rotation APIs are available and covered by regression tests. `IdentityChokeAction` is now produced directly by the identity rotation path and exposed through `BtDownloadCommand`; legacy `usize` action/hooks remain only as compatibility boundaries while the seeder/leecher snapshot implementations retain indices solely for mutating the caller-owned peer slice. Stable-identity execution entry points now propagate through `BtDownloadCommand`, `PeerStorage`, and both seeder/leecher choke managers; the slice itself remains the final mutation boundary for compatibility with the current connection registry.

### 14. SFTP

**Status:** Partial
**Rust files:** `engine/sftp_download_command.rs`, `aria2-protocol/src/sftp/`
**Notes:** SFTP now validates that the local output is not longer than the remote file, resumes from the existing local prefix with positioned remote reads and offset-aware writes, rejects premature remote EOF instead of reporting success, and propagates local finalize errors. Full control-file/Segment-driven resume, SFTP connection pooling/finish-command parity, and end-to-end mock-server coverage remain outstanding.

### 15. aria2-next Enhancements

**Note:** These items are NOT required for original aria2 compatibility, but are DESIRED for aria2-next compatibility.

#### ED2K/eDonkey Protocol (P2)

| Component | Status | Notes |
|-----------|--------|-------|
| All ED2K components | Missing | 46 new source files in aria2-next |

#### spdlog Rotating Logs (P1)

| Component | Status | Notes |
|-----------|--------|-------|
| `--log-max-size` / `--log-max-files` | Fixed | Size-based rotation with bounded numbered backups is implemented in `aria2-core/src/log.rs`; options are wired through application startup. |
| `trace` log level | Fixed | `parse_log_level()` maps `trace` to `tracing::Level::TRACE`. |
| Rotating file sink | Fixed | `SizeRotatingWriter` rotates before an overflowing write and keeps the configured backup window. |

#### HTTP Tail Segment Reclaim (P1)

| Component | Status | Notes |
|-----------|--------|-------|
| HttpTailReclaimPolicy | Complete | `engine/http_tail_reclaim.rs` |
| DownloadCommand tail tracking | Complete | `engine/download_command/tail_reclaim.rs` |
| Per-connection stall tracking | Complete | `http/tail_reclaim/tracker.rs` |

#### Default Value Audit (P0)

| Option | Old Default | New Default | Status |
|--------|-------------|-------------|--------|
| `--file-allocation` | `prealloc` | `prealloc` | Original-compatible baseline restored on 2026-08-09 |
| `--max-connection-per-server` max | 16 | 1024 | Adopted |

#### Decimal Size Parsing (P1)

| Component | Status | Notes |
|-----------|--------|-------|
| `parse_size_str()` | Complete | Already supports `f64` + suffix |
| `paramed_string` expansion | Fixed | Rust selector expander now supports C++-compatible numeric ranges, alphabetic ranges, choice expansion, step values, padding, and Cartesian products. |

#### Other aria2-next Items

| Item | Status |
|------|--------|
| `reduceActiveDownloadsToLimit()` | Fixed | RequestGroupMan reduces excess active groups on runtime max-concurrent decreases, setting pause and restart state so they can be re-promoted when capacity returns. |
| Peer rename (`bad peer` -> `temporarily rejected peer`) | NOT Adopted |
| `--detach-share-only` rename | Fixed | Rust exposes the original-compatible `bt-detach-seed-only` option and propagates it into RequestGroup; completed BT payloads enter seed-only mode only when enabled and no longer consume the active-download budget. |
| Content-Disposition parser edge cases | Fixed | Strict RFC state-machine behavior now matches aria2_original, including rejection of trailing empty parameters and malformed extended values, with parity regressions covered. |
| `isChecksumVerificationPending()` | Fixed | DownloadContext exposes an atomic whole-file verification-pending state, and RequestGroup download-result generation reports completion only after verification. |
| `SeedCheckCommand` without btRuntime | DONE |
| Hostname-based socket pooling | Fixed | HTTP pool keys now include scheme, hostname, port, and proxy identity; acquisition resolves all DNS addresses and tries candidates in order, matching C++ multi-address fallback. |
| Control file removal on user halt | Fixed | RequestGroup stores the sidecar path; HTTP/concurrent, generic segmented, and BitTorrent production paths now register it before transfer, and removal is idempotent with NotFound ignored and other filesystem failures reported. |

---

## Summary: P0 Items (Must Fix)

| # | Module | Issue | Status |
|---|--------|-------|--------|
| 1 | Metalink | Priority direction reversed | FIXED |
| 2 | Metalink | PostDownloadHandler missing | FIXED |
| 3 | Metalink | Multi-file Metalink rejected | FIXED |
| 4 | FileAllocation | FileAllocationMan queue missing | FIXED |
| 5 | FileAllocation | FileAllocationCommand loop missing | FIXED |
| 6 | Cookie | `to_netscape_line` domain dot inversion | FIXED |
| 7 | LPD | BEP14 message format mismatch | FIXED |
| 8 | WebSocket | `rpc-max-request-size` OOM | FIXED |
| 9 | aria2-next | Default `--file-allocation: trunc` | Superseded | The migration target is the aria2_original default `prealloc`; Rust now uses that value consistently across HTTP, FTP, BitTorrent, and Metalink constructors. |

---

## Summary: P1 Items (Feature-Breaking)

*Re-audited 2026-08-01 against current code: 25 of 28 P1 items done/fixed, 2 N/A, 1 architectural; only 3 aria2-next enhancement items remain missing.*

| # | Module | Issue | Status |
|---|--------|-------|--------|
| 1 | BT | BtSetup orchestrator missing | DONE | `engine/bt_setup.rs` implements `BtSetup::setup()` wiring registry/announce/runtime |
| 2 | BT | DefaultBtMessageFactory context injection | DONE | BEP 5 Port messages received in the real download path (`wait_for_piece_block` / `wait_for_any_piece_block`) now add `(peer_ip, port)` as a DHT node via spawned `add_node` (ping is synchronous, so it runs detached). `dht_engine` threaded from `download_pieces_loop` through `download_piece_blocks(_endgame)` → `request_block(_endgame)` → wait loops. The orphan `BtPeerInteraction::dispatch_message` Port branch (no production callers) was left as-is |
| 3 | BT | Write Disk Cache integration | FIXED | BT single-file downloads now write through `CachedDiskWriter` (positioned `write_at` + 16 MB write-back cache). **This also fixed a silent data-corruption bug**: the old code used sequential `DefaultDiskWriter::write()`, but BT downloads pieces out of order (RarestFirst), so pieces landed at the wrong offsets; web-seed fallback had the same bug. `ThrottledWriter` gained a `SeekableDiskWriter` impl (rate limiting preserved), plus `Box<dyn SeekableDiskWriter>` blanket impl. 2 regression tests |
| 4 | BT | Seed phase tracker communication | DONE | `BtSeedManager` now carries a `TrackerAnnouncer` and re-announces at the tracker-provided interval inside `run_seeding_loop` (C++ SeedCheckCommand behaviour); `run_seeding_phase` builds the announcer from the torrent announce list and passes the real info hash + generated peer id (`new_with_announcer`) |
| 5 | Metalink | Chunk-level piece hash verification | FIXED | `verify_pieces()` chunk-checks downloaded data against `<pieces>`; parser fixed to support v4 `<hash>` children AND v3 concatenated text (previously `split_whitespace` produced one bogus entry), `piece_count()` corrected |
| 6 | Metalink | Version/language/OS filtering | DONE | `parser.rs` query_entries + `metalink_to_request_group.rs` builder filters |
| 7 | Metalink | Torrent metaurl handling | PARTIAL | `MetalinkToRequestGroup` and `MetalinkDownloadCommand` preserve metaurl-only files; direct execution falls back to downloading the `.torrent` and running `BtDownloadCommand` when mirrors fail. This remains a command-local fallback, not yet the complete C++ `BtDependency` graph: Metalink conversion still does not create an independent metadata RequestGroup or wire the new `BtDependency` into the manager. Rust now has a `MetadataInfo` value model, parent metadata provenance, a reusable torrent-to-`DownloadContext` builder, and `BtDependency` context injection with both in-memory and file-backed metadata tests. `BtDependency` can now resolve a configured metadata file when its prerequisite completes. The task spawner now routes resolved `bt://` payload groups through an externally-owned `BtDownloadCommand`, preserving the manager's group and context. JSON ResumeData now persists and restores the resolved metadata path through `bt_saved_metadata_path` and `MetadataInfo`; standard session serialization remains unchanged. Production Metalink graph construction, manager insertion, and automatic cross-restart dependency reconstruction remain missing. `FileDownloadInfo.torrent_metaurls` is populated in single and multi-file modes |
| 8 | Metalink | Metalink v3 `verification` element | DONE | Parser and production Metalink command consume v3 `<verification>` hashes/pieces, select the strongest whole-file hash, and fail over mirrors on verification mismatch |
| 9 | FileAllocation | BtFileAllocationEntry missing | FIXED | BT downloads pre-allocate through `FileAllocationMan` (single + multi-file) |
| 10 | FileAllocation | HttpFileAllocationEntry missing | FIXED | HTTP `DownloadCommand` pre-allocates through `FileAllocationMan` |
| 11 | WebSocket | Notification format differs from C++ | DONE | `websocket.rs` `new_with_gid` emits `params:[{"gid":...}]`, test-verified against C++ format |
| 12 | Cookie | DomainNode tree missing | DONE | `storage.rs` domain buckets + `BTreeSet` LRU tracker mirror C++ design |
| 13 | Cookie | LRU tracker missing | DONE | `storage.rs` lru_tracker |
| 14 | Segment | SegmentMan::advertisePiece() not wired | DONE | HTTP path advertises via `complete_segment`; BT path was already covered by `BtPeerInteraction::broadcast_have` (piece_download.rs) — a HAVE broadcast to every peer on piece completion, the functional equivalent of C++ `advertisePiece()`; the PARTIAL flag was an audit miss |
| 15 | Segment | SegmentMan::getSegment(FileEntry) missing | DONE | `segment_man_support.rs` `get_segments_for_file_entry` |
| 16 | Segment | initStorage() auto-initialization | N/A | `SegmentMan` has **zero callers** in the download paths (HTTP writes files directly, BT uses PieceManager) — an orphan module like the old PieceStorage validators; auto-creating a DiskAdaptor nobody consumes is dead work |
| 17 | DHT | DHT not wired into BtDownloadCommand | DONE | `peer_management.rs` `discover_peers` calls `engine.find_peers()` (BEP 27 private-torrent disabled) |
| 18 | FTP | Active mode (PORT/EPRT) missing | DONE | `ftp/connection/connector.rs` active_mode (EPRT→PORT fallback) |
| 19 | CheckIntegrity | CheckIntegrityMan queue missing | FIXED | `checksum/check_integrity/man.rs` adds the queue + background worker (mirrors C++ CheckIntegrityMan + CheckIntegrityDispatcherCommand + CheckIntegrityCommand): chunked `validate_chunk` with `yield_now` between chunks, sequential by default, `oneshot` outcome notification, cancellation. `FileChunkValidator` handles single-file data and `MultiFileChunkValidator` now reads the logical torrent stream across physical files, including pieces crossing file boundaries. Wired into BT (single + multi-file) and HTTP (when the context has piece hashes e.g. Metalink); `DownloadOptions.check_integrity` added and parsed from `--check-integrity` in apply.rs + RPC + session. Cross-file pass/corruption tests included. BT integrity outcomes now preserve verified piece indexes and pre-seed the BT picker, while mismatched pieces are left selectable for re-download; `BT now propagates `hash-check-only` and terminates after validation without peer discovery; mismatch returns a check-only failure. HTTP integrity now short-circuits a fully verified existing output, while BT continues to pre-seed verified pieces and leaves failed pieces selectable. Full `StreamCheckIntegrity`/`BtCheckIntegrity::onDownloadIncomplete()` command dispatch, PieceStorage result synchronization, and active BT read-only reopening remain separate gaps; active HTTP and BT integrity paths now truncate oversized single-file outputs and BT multi-file entries before validation. Request-group parent/child relation writes are now idempotent; Metalink/BT post-download children retain `following`/`belongsTo`, while demotion records parent `followedBy`. Full Metalink internal torrent dependency graph and session restoration remain separate gaps. |
| 20 | aria2-next | spdlog rotating logs | FIXED | `SizeRotatingWriter`, `log-max-size`, `log-max-files`, and TRACE mapping |
| 21 | aria2-next | reduceActiveDownloadsToLimit() | DONE | `request_group_man/promotion.rs` `reduce_to_limit`, called from engine_loop |
| 22 | aria2-next | Content-Disposition edge cases | FIXED | Strict parser state-machine parity tests |
| 23 | aria2-next | SeedCheckCommand without btRuntime | DONE |
| 24 | Engine | Output-file control file lifecycle | PARTIAL | Binary output `.aria2` is saved/removed by sequential and concurrent paths; streaming creation can degrade with warning, legacy concurrent creation errors propagate. `ControlFile::load` now rejects truncated headers, incomplete/unknown checksums, bitfield length mismatches, and completed lengths beyond total length without panicking. Pause/remove/fallback force progress saves; errored downloads retain the file. RequestGroup active remove now waits for command completion before terminal demotion, while timeout records a structured `TimeOut`; force remove/force halt and timeout inject a synthetic `Cancelled` completion before Tokio abort; completion processing de-duplicates by command generation rather than GID, so multiple commands under one RequestGroup each decrement `num_commands` independently; terminal Complete/Error/Removed state is deferred until the final command completion, while earlier failures only update the error snapshot; retry/promotion clears command_failure, and mapped codes preserve timeout/cannot-resume/404/checksum outcomes; HTTP 401/407 map to `HttpAuthFailed`, 502/503/504 map to `HttpServiceUnavailable`, ordinary HTTP failures map to `HttpProtocolError`, and redirect limits map to `HttpTooManyRedirects`; the numeric `DownloadResultCode` table now matches C++ (`ChecksumError=32`, `Removed=31`), with only Rust-local `Paused=33` beyond the wire table. Wired RPC stopped queries, status fallback, removal, purge, and stopped counts now use the core `RequestGroupMan` result store; full RPC/core result precedence and forced-termination end-to-end evidence remain incomplete; direct protocol/file classification is now covered for FTP/HTTP errors, 404/503, disk space, file I/O, and option errors; JSON, Metalink, Bencode, BitTorrent, and Magnet parse failures now carry dedicated structured variants. HTTP header/status 与 FTP PASV 错误已细分为 protocol error，主要 disk writer 的目录创建、文件创建/打开已细分，控制文件读写使用 FileIo；DNS cache 失败已统一返回结构化 `Aria2Error::NameResolve`，task spawner 对 HTTP/HTTPS/FTP/FTPS promotion 前解析失败会通过 generation completion 进入 `NameResolveError`；DNS cache 正负缓存按 `(hostname, port)` 隔离并保留 Good/Bad 候选状态；FTP control connect 在拨号前保存真实候选，首次失败可准确标坏，候选耗尽后重新解析，协议及 data-transfer 错误不会误淘汰 control 地址；raw HTTP manager 已有 direct-origin peer discard/idle eviction，但 reqwest 生产路径尚无 selected-peer callback；BT pool 使用独立 SocketAddr 身份，不参与 DNS 淘汰，piece-hash 责任 peer 回调仍缺失；io_uring 的 open/read/write/truncate/flush/close 已细分为 FileOpen/FileIo；file-lock acquire 已返回结构化 Aria2Error 并区分 FileCreate/DirCreate/FileIo；HTTP redirect/auth 分类和完整 RPC/core precedence remain incomplete. Download writer finalization now propagates errors across HTTP, Metalink, and SFTP, and the default sequential writer performs `sync_all` before close. Graceful sequential cancellation finalizes before saving progress, concurrent cancellation drains pending write chunks and flushes before saving, and the BitTorrent piece loop observes halt requests before selecting more work. Timeout now requests graceful halt instead of aborting the task, while explicit force halt and final engine teardown still use immediate abort by design; end-to-end force-abort flush/save and RPC/core stopped-result de-duplication still lack evidence. Session `{gid}.aria2` JSON persistence is a separate lifecycle and format. |
| 25 | Engine | AuthConfigFactory centralized factory | DONE | `http/auth.rs` `AuthConfigFactory`, used by auth challenge handler + auth_retry |
| 26 | Engine | poolSocket()/popPooledSocket() for FTP | N/A | `FtpConnectionPool` covers pooling; reqwest pool covers HTTP |
| 27 | Engine | validateToken() HMAC token validation | FIXED | `server.rs` `verify_token` now uses `constant_time_eq` for token comparison (prevents timing side-channel). `RpcAuthMiddleware` present. Matches C++ security semantics |
| 28 | Option | 48 C++ option handlers not registered | DONE | 232 `.register(` calls in option_definitions (>= C++ 212) |

---

## Summary: P2 Items (Minor)

| # | Module | Issue | Status |
|---|--------|-------|--------|
| 1 | BT | Zero-copy Piece optimization | PARTIAL | Completed piece payloads use `Bytes` through the disk-writer/cache boundary; protocol block aggregation still uses mutable buffers. |
| 2 | BT | addAllowedFastMessageToQueue() always empty | FIXED | BEP6 `send_allowed_fast_for_torrent()` computes the canonical address/info-hash fast set, queues messages, records peer state, and flushes after handshake. |
| 3 | BT | createFastIndexBitfield() | FIXED | `DefaultPieceStorage::get_missing_fast_pieces()` intersects the peer bitfield with local missing/unused pieces and the peer AllowedFast set before selection. |
| 4 | Checksum | Adler32 streaming | Partial |
| 5 | Cookie | Per-domain cookie limit | FIXED | `CookieStorage` enforces the C++ 50-cookie domain limit, expires stale entries first, then replaces the least-recently-accessed cookie. |
| 6 | Cookie | SQLite cookie parser | FIXED | `Sqlite3CookieParser` + Mozilla/Chromium schemas implemented in `http/sqlite_cookie_parser.rs` (rusqlite bundled); `CookieStorage::load_file` auto-detects SQLite magic vs Netscape |
| 7 | Cookie | Duplicate Cookie/JarCookie structures | PARTIAL | HTTP download executors and session persistence now use canonical `CookieStorage`/Netscape storage; `JarCookie` remains only for legacy JSON/session/API compatibility, so full model unification remains open. |
| 8 | HTTP | eraseConfidentialInfo() | FIXED | `http::auth::erase_confidential_info()` masks Authorization, Proxy-Authorization, Cookie, and Set-Cookie values before logging, with pipeline regressions. |
| 9 | LPD | Multicast interface config | FIXED | `LpdAnnouncer::with_interface()` joins the BEP14 group on the selected IPv4 interface and exposes the effective configuration through `interface()`. |
| 10 | WebSocket | Extra onBtCacheChanged event type | Present |
| 11 | FileAllocation | Allocation progress events | FIXED |
| 12 | aria2-next | ED2K/eDonkey protocol | Missing |
| 13 | aria2-next | Peer rename | NOT Adopted |
| 14 | aria2-next | --detach-share-only rename | NOT Adopted |
| 15 | aria2-next | isChecksumVerificationPending() | Missing |
| 16 | aria2-next | Hostname-based socket pooling | Fixed | HTTP key includes scheme/host/port/proxy and resolver iterates all candidate addresses with fallback. |
| 17 | DHT | Entry point DNS resolution | Partial |

---

## Rust Advantages Over C++

| # | Area | Advantage | Details |
|---|------|-----------|---------|
| 1 | Memory safety | No use-after-free, no buffer overflow | Rust's ownership system eliminates C++ bug classes |
| 2 | Concurrency | DashMap, broadcast::channel | Thread-safe by design; no manual mutex |
| 3 | Credential security | Secret<T> auto-zeroing | Automatic memory zeroing on drop |
| 4 | Async I/O | tokio async/await | No manual event loop or state machines |
| 5 | Redirect safety | Iterative redirect following | Prevents stack overflow |
| 6 | Connection management | HttpConnectionManager LRU pool | Built-in connection pooling with eviction |
| 7 | Happy Eyeballs | RFC 8305 dual-stack racing | Not in C++ |
| 8 | Zero-copy I/O | Linux splice(2) | Kernel-level zero-copy |
| 9 | SOCKS proxy | SOCKS4/SOCKS5 native support | C++ relies on external proxy |
| 10 | Conditional GET | If-Modified-Since / ETag | Not in C++ |
| 11 | Magic byte detection | detect_encoding_from_magic_bytes() | Not in C++ |
| 12 | Saturating arithmetic | BtRuntime connection counts | Prevents negative count bugs |
| 13 | Binary-compatible progress | BtProgressInfoFile | Reads C++ v0/v1 format |
| 14 | Notification batching | NotificationBatcher | Deduplication + time-based flush |
| 15 | BtAnnounce health tracking | Exponential backoff + health scoring | Beyond C++'s linear retry |
| 16 | Anti-flooding | BtMessageDispatcher | Beyond C++'s basic choke management |
| 17 | File allocation | Secure-falloc | Zero-fill on macOS/Windows |
| 18 | Mmap I/O | MmapDiskWriter | Memory-mapped file I/O |
| 19 | FTPS/TLS | tokio_rustls integration | Modern TLS stack |
| 20 | Progress aggregation | AtomicProgress + SpeedSmoother | Lock-free EMA speed smoothing |
| 21 | DHT engine | Owned DhtEngine struct | No global mutable state |

---

## Architectural Decisions

### HTTP Client: reqwest/hyper vs Raw Sockets

**Connection-failure boundary (2026-08-04):** Protocol-neutral `EndpointKey` and `ConnectionContext` keep the logical `(hostname, port)` separate from the selected socket peer. The raw HTTP manager can discard an active connection or evict matching idle direct-origin connections; proxy pooled connections are deliberately excluded from origin eviction. FTP constructs the context before `connect`, rejects only failed control-connection candidates, and refreshes DNS after the good candidate set is exhausted. BitTorrent pools use `SocketAddr` peer identity and do not share DNS bad-address semantics. The piece download path now returns stable concrete source peer addresses and temporarily rejects a source IP only when every verified block came from that one peer; mixed-source or unknown-source pieces reject no peer to avoid false positives. The rejection state is shared through the BT registry so piece verification, tracker/DHT/public-tracker/LPD discovery, and PEX connection attempts use one download-scoped state; block-level attribution can be added when the scheduler preserves block responsibility. The reqwest-based download path still does not expose a reliable selected peer to the engine, so it must not call DNS bad-address eviction until a connector-level peer callback exists.

**Pros:** Robust HTTP/1.1 and HTTP/2, TLS via rustls, connection pooling built-in
**Cons:** Less control over incremental parsing; harder to implement HttpRequestEntry tracking

### FTP: Async vs State Machine

**Pros:** Simpler code, natural flow, all negotiation steps implemented
**Cons:** Cannot pause/resume mid-negotiation

### DHT: Owned Engine vs Global Singleton

**Pros:** No global mutable state, easier testing, clear ownership
**Cons:** Need explicit wiring into BT download flow

### Stream Filter: Trait Objects

**Pros:** Safe dynamic dispatch, streaming decompression for all codecs
**Cons:** Minor heap allocation per filter (negligible vs I/O cost)

---

## Recommended Fix Order

### Phase 1: P0 Fixes (Protocol/Security) -- ALL DONE (2026-07-31)

1. ~~FileAllocationMan queue~~ -- FIXED: `filesystem/file_allocation_man.rs` real queue + worker
2. ~~FileAllocationCommand loop~~ -- FIXED: background worker loop with chunked cooperative allocation

### Phase 2: P1 Fixes (Feature Completeness) — status 2026-08-01

3. ~~BtSetup orchestrator~~ — DONE (pre-existing)
4. ~~DHT announce/lookup wiring into BtDownloadCommand~~ — DONE (pre-existing)
5. ~~CheckIntegrityMan queue + CheckIntegrityDispatcherCommand~~ — FIXED 2026-07-31 (`check_integrity/man.rs`)
6. ~~CookieStorage DomainNode tree~~ — DONE (pre-existing)
7. Metalink chunk-level piece hash is complete; torrent metaurl handling remains PARTIAL (2026-08-05): metadata persistence and context injection are implemented, but the independent metadata RequestGroup dependency graph is not.
8. ~~FTP active mode (PORT/EPRT)~~ — DONE (pre-existing)
9. ~~Write Disk Cache integration~~ — FIXED 2026-07-31 (CachedDiskWriter write_at + 16MB cache)
10. ~~Seed phase tracker communication~~ — DONE (BtSeedManager re-announce)
11. ~~AuthConfigFactory centralized factory~~ — DONE (pre-existing)
12. ~~aria2-next: spdlog rotating logs~~ — FIXED 2026-08-05 (`SizeRotatingWriter`, `log-max-size`, `log-max-files`, and TRACE mapping)
13. ~~aria2-next: reduceActiveDownloadsToLimit()~~ — DONE (pre-existing)
14. ~~aria2-next: Content-Disposition edge cases~~ — FIXED 2026-08-01 (RFC 6266 trailing `;` acceptance; fictional `CD_VALUE_COMPLETE`/`CD_FINAL_EMPTY_PARAMETER_ALLOWED` state names removed — these were documentation artifacts, not real C++ states)
15. ~~aria2-next: SeedCheckCommand without btRuntime~~ — DONE (BtSeedManager already uses CancellationToken; dead BtRuntime code removed)
16. ~~aria2-next: Control file removal on user halt~~ — DONE (pre-existing)
17. ~~WebSocket notification format alignment~~ — DONE (pre-existing)
18. ~~SegmentMan advertisePiece + getSegment(FileEntry)~~ — DONE (pre-existing)
19. ~~Selector stat_man unification~~ — FIXED 2026-08-01 (`ServerStatMan::shared()` singleton)
20. ~~Global rate limiter wiring~~ — FIXED 2026-08-01 (all paths: HTTP/BT/FTP/SFTP/Metalink/Magnet; `ThrottledWriter` dual-bucket serial acquire)
21. ~~Token constant-time comparison~~ — FIXED 2026-08-01 (`constant_time_eq`)
22. ~~Session GID zero-padding~~ — FIXED 2026-08-01 (`{:016x}`)
23. ~~Request pause/unpause/remove scheduling~~ — FIXED 2026-08-01 (`requeue_non_terminal_groups`)

### Phase 3: P2 Fixes (Quality of Life)

19. SQLite cookie parser
20. Cookie structure consolidation
21. LPD multicast interface config
22. Zero-copy Piece optimization
23. Adler32 streaming
24. aria2-next: Peer rename
25. aria2-next: --detach-share-only rename
26. aria2-next: isChecksumVerificationPending()
27. aria2-next: Hostname-based socket pooling
28. aria2-next: ED2K protocol (large scope, low priority)
29. Missing option handlers (48 remaining)
