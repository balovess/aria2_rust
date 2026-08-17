# Compatibility Status

Last verified: 2026-08-17
Reference implementation: aria2_original/  
Rust workspace version: 0.3.0
Public product identity: aria2-rust 0.3.0

All product-version surfaces are owned by this workspace release source:
CLI `--version`, the startup banner, RPC `aria2.getVersion`, default HTTP and
BitTorrent identities, SDK metadata, distribution metadata, and installer
fallbacks resolve to `aria2-rust 0.3.0`. Protocol-format versions such as JSON-
RPC `2.0`, Metalink `3.0`/`4.0`, SFTP `3`, and internal persistence-format
versions are intentionally separate from product identity.

This is the current status source for the migration. The file-level records
under docs/migration/ and the historical plans under .trae/ are useful
evidence, but their checklists do not establish behavioural compatibility.

## Status Rules

The Goal control vocabulary is `missing`, `partial`, `compatible`, and
`verified`. The existing uppercase labels in the matrix are retained as a
readable audit projection: `MISSING` maps to `missing`, `PARTIAL` and
`UNVERIFIED` map to `partial`, `FULL` maps to `compatible` until the required
Rust-owned protocol/E2E evidence is independently reproducible, and `N/A`
maps to an intentional Rust-native replacement. Historical module records may
use their original audit labels; the current matrix and Goal control are the
authoritative status surface.

| Status | Meaning |
| --- | --- |
| FULL | Rust covers the relevant original behaviour and has focused protocol or end-to-end evidence. |
| PARTIAL | The main path exists, but a documented behaviour, lifecycle, platform, or interoperability gap remains. |
| UNVERIFIED | Code exists, but the required protocol/E2E evidence has not been run or is not reproducible on the current host. |
| MISSING | No Rust equivalent exists for an in-scope original capability. |
| N/A | The original implementation is replaced by an intentional Rust/platform mechanism and the replacement is documented. |

“Source file compared” is not the same claim as FULL. The 530 C++ source
units catalogued in docs/MIGRATION.md measure audit coverage only.

## Compatibility Policy

The public behavior of `aria2_original` is a hard compatibility contract. This
includes CLI and configuration names/defaults, JSON-RPC/XML-RPC/WebSocket wire
shapes, authentication, error codes and HTTP status codes, session files,
notifications, task states, and the behavior relied on by existing clients
such as browser extensions. A Rust implementation is not compatible merely
because it exposes a similarly named method.

Product identity is an intentional documented exception: `aria2.getVersion`,
the CLI version action, default User-Agent values, and BitTorrent peer identity
identify `aria2-rust` at the current workspace version. Their surrounding wire
formats and protocol behaviour remain compatible.

Internal modules may be redesigned around Rust ownership, typed errors,
async I/O, and lower allocation or lock overhead, but those changes must stay
behind the public seam. A Rust-only feature is an extension: it must be
additive, explicitly documented, and must not change the result observed by an
original client using an original request. Features absent from
`aria2_original` (for example FTPS) are measured as extensions and are not
substitutes for missing original behavior.

## Goal Control

| Field | Current value |
| --- | --- |
| Active phase | `phase-2-core-domain` (`in_progress`) |
| Passed and locked phases | `phase-1-baseline-matrix` (`passed_locked`) |
| Latest verification | 2026-08-17 in the current worktree based on `1a8641c` (`dev`), including the full all-features workspace library/test run, the C API focused tests, the Windows `aria2-core` cdylib/import-library build, FTP/SFTP retry-wait cancellation regressions, the active-slot drain regression, and the shutdown-lifecycle, session-restart, control-path, session-iterator-error, session-stale-file, session-comment, stopped-result, piece-storage, digest-count integrity, control-file, RequestGroup lifecycle, terminal-progress, and pause/completion-race slices below; unrelated in-progress edits are preserved |
| Current status | `PARTIAL`; phase 1 is locked, but the final acceptance and stop conditions are not met |

## 2026-08-17 RequestGroup Active-Slot Drain Checkpoint

The scheduler now counts every non-seed RequestGroup that remains in the active
map until its final command completes. A paused or terminal-status group can
therefore drain its in-flight command without releasing a
`max-concurrent-downloads` slot early and allowing another group to be
promoted over the configured limit. The slot is released only when the normal
requeue or terminal demotion removes the group from the active scheduling
store.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib request::request_group_man -- --test-threads=1
  37 passed, 0 failed
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3452 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

This closes only the paused-command concurrency-slot boundary. The active
phase remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; broader lifecycle combinations, protocol interoperability,
bindings, measured performance evidence, and final workspace acceptance remain
open.

## 2026-08-17 HTTP Existing-Payload Integrity Recovery Checkpoint

The HTTP `check-integrity` path now has Rust-owned end-to-end evidence for a
corrupt existing payload. Piece-hash validation detects the mismatch, discards
the untrusted resume state, downloads the payload again, and reaches
`Complete` with the expected bytes. A non-empty unknown piece-hash algorithm is
rejected explicitly instead of silently falling back to SHA-1; the empty type
continues to use the legacy SHA-1 default.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_download test_e2e_http_check_integrity -- --test-threads=1
  3 passed, 0 failed
~~~

This closes only the corrupt-existing-HTTP-payload recovery boundary. The
active phase remains `phase-2-core-domain` (`in_progress`) and the migration
remains `PARTIAL`; broader lifecycle combinations, third-party and original
client interoperability, bindings, measured performance evidence, and final
workspace acceptance remain open.

## 2026-08-17 Zero-piece storage safety checkpoint

`BitfieldMan` and `DefaultPieceStorage` now handle zero-piece and zero-piece-
length inputs without division or underflow panics. Empty and mismatched
bitfields are rejected without mutating completion, use-bit, or piece-stat
state. Normal piece selection and bitfield loading behavior is unchanged.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib segment::piece_storage -- --test-threads=1
  76 passed, 0 failed
~~~

This closes only the zero-piece and malformed-bitfield storage boundary. The
active phase remains `phase-2-core-domain` (`in_progress`) and the migration
remains `PARTIAL`; broader lifecycle combinations, protocol interoperability,
bindings, measured performance evidence, and final workspace acceptance remain
open.

## 2026-08-17 C API cdylib verification checkpoint

The Rust-owned C API now has reproducible build and export evidence in addition
to its focused in-crate lifecycle tests. The all-features `aria2-core` cdylib
build produced `target/debug/aria2_core.dll` and its Windows import library;
the import library exports all 19 `aria2_rust_*` functions declared by
`bindings/c/include/aria2_rust.h`. This verifies the current opaque-handle C
surface is linkable on the host. It remains a source-level Rust interface and
is intentionally not binary-compatible with `aria2_original`'s C++ classes,
`std::string`, `std::vector`, or virtual-dispatch ABI.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features c_api --lib -- --test-threads=1
  3 passed, 0 failed
cargo build -p aria2-core --all-features
  PASS; target/debug/aria2_core.dll and target/debug/aria2_core.dll.lib produced
llvm-nm.exe target/debug/aria2_core.dll.lib | Select-String 'aria2_rust_'
  19 exported aria2_rust_* entry points
clang.exe temporary C consumer + target/debug/aria2_core.dll.lib
  compiled, linked, and ran successfully with exit code 0
~~~

This closes only the current-host C API header/library integration gate. C
callers still need platform-specific ABI checks and the complete original
`aria2api.h` semantic comparison before the public C API row can move beyond
`PARTIAL`.

## 2026-08-17 FTP/SFTP Retry-Wait Cancellation Checkpoint

FTP and SFTP retry backoff now observes the owning `RequestGroup` lifecycle
flags in bounded intervals. A paused, removed, or halted task therefore exits
the retry wait promptly instead of remaining asleep for the full configured
`retry-wait` duration. Ordinary retry timing and the existing total-attempt
`max-tries` contract are unchanged. The change is Rust-native lifecycle
handling behind the existing public options and does not change CLI/config
names, defaults, session format, RPC wire values, protocol wire values, or
product identity.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib engine::ftp_download_command::tests -- --test-threads=1
  23 passed, 0 failed
cargo test -p aria2-core --all-features --lib engine::sftp_download_command::tests -- --test-threads=1
  17 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_ftp_download -- --test-threads=1
  36 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --test test_e2e_sftp_download -- --test-threads=1
  23 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3451 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes only the FTP/SFTP retry-wait lifecycle boundary. Third-party
servers, original-client interoperability, broader cross-protocol lifecycle
combinations, bindings, measured performance evidence, and final workspace
acceptance remain open. A source audit also confirms that the standalone
`HttpResponseProcessor` containing a hardcoded skip-handler retry wait has no
production caller; its retryable/fatal result distinction remains an isolated
unverified adapter gap rather than a production configuration change.

## 2026-08-17 Session Directory Iterator Error Checkpoint

Session loading and full session cleanup now propagate errors returned while
advancing `tokio::fs::ReadDir`, instead of silently treating an enumeration
failure as end-of-directory and returning success. The existing stale-file
pruning helper already propagated this error class and remains unchanged.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib session::session_persistence -- --test-threads=1
  20 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_session -- --test-threads=1
  13 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes only the session-directory iterator error-reporting boundary. The
active phase remains `phase-2-core-domain` (`in_progress`) and the migration
remains `PARTIAL`; broader restart and control-file behavior, lifecycle
combinations, cross-protocol interoperability, bindings, performance
evidence, and final workspace acceptance remain open.

## 2026-08-17 Session Snapshot and Control-Path Restart Checkpoint

The production text-session path now writes an empty atomic snapshot when no
persistable groups remain, and application shutdown no longer skips that write
when the group manager is empty. This prevents removed or completed entries
from surviving in the previous `save-session` file and reappearing on restart.
The Rust-owned A2CF control-file helper now derives the public sidecar path as
`output.aria2` rather than the accidental `output..aria2`; a direct path
contract test covers the distinction.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib session::active_session -- --test-threads=1
  10 passed, 0 failed
cargo test -p aria2 --all-features --lib app::tests -- --test-threads=1
  22 passed, 0 failed
cargo test -p aria2-core --all-features --lib filesystem::control_file -- --test-threads=1
  14 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_disk_io -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo fmt --all -- --check
  PASS
~~~

This closes the one-file stale-session and Rust-owned sidecar-path boundaries
only. The active phase remains `phase-2-core-domain` (`in_progress`) and the
migration remains `PARTIAL`; true multi-process restart tests, original-binary
control-file interoperability, broader lifecycle combinations, cross-protocol
interoperability, bindings, performance evidence, and final workspace
acceptance remain open.

## 2026-08-17 Shutdown Resume-State Checkpoint

The engine's process shutdown signal now uses `HaltReason::ShutdownSignal`
instead of the user-removal reason. Active downloads therefore remain
resumable and produce the `IN_PROGRESS` result mapping during cleanup, rather
than being incorrectly marked `REMOVED` and omitted from the next session.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib engine::engine_loop::tests -- --test-threads=1
  17 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the shutdown-reason mapping boundary only. The active phase
remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; protocol-specific cancellation, retry/resume combinations,
cross-process and cross-protocol interoperability, bindings, performance
evidence, and final workspace acceptance remain open.

## 2026-08-17 Session Stale-File Checkpoint

`SessionPersistence::save_state` now treats a successfully written snapshot as
authoritative and removes older per-download `.aria2` files that are absent
from the current group set. This prevents removed or completed tasks from
reappearing after restart. If a current entry cannot be serialized or written,
stale-file pruning is skipped so the previous recoverable snapshot remains
available.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib session::session_persistence -- --test-threads=1
  20 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the stale per-download session-file boundary only. The active
phase remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; broader restart, control-file, lifecycle, cross-protocol
interoperability, bindings, performance evidence, and final workspace
acceptance remain open.

## 2026-08-17 RequestGroup Terminal-Progress Checkpoint

The interior-mutable `RequestGroup::mark_complete` path now sets
`completed_length` to the group's total length before publishing the terminal
completion event. This matches `complete(&mut self)` and keeps engine-driven
completion and stopped-result snapshots at 100 percent instead of retaining a
stale partial progress value. The earlier MultiDiskAdaptor write-range change
was intentionally reverted after its existing tests confirmed that declared
stream-boundary truncation is part of the current Rust-owned contract.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib filesystem::multi_disk_adaptor -- --test-threads=1
  44 passed, 0 failed
cargo test -p aria2-core --all-features --lib request::request_group -- --test-threads=1
  117 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the interior-mutable terminal-progress consistency boundary only.
The active phase remains `phase-2-core-domain` (`in_progress`) and the
migration remains `PARTIAL`; broader lifecycle combinations, session and
control-file variants, cross-protocol interoperability, bindings, performance
evidence, and final workspace acceptance remain open.

## 2026-08-17 Session Comment Boundary Checkpoint

Session-file comments are now ignored without terminating the current entry.
Previously, a comment between a URI and later properties caused the parser to
flush the entry early, silently dropping the remaining options. Blank lines
remain the entry separator, preserving the existing aria2 session format.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib session::session_serializer -- --test-threads=1
  11 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_session -- --test-threads=1
  13 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the in-entry session-comment parsing boundary only. The active
phase remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; broader session restart semantics, lifecycle combinations,
cross-protocol interoperability, bindings, performance evidence, and final
workspace acceptance remain open.

## 2026-08-17 Full Core Test-Target Checkpoint

The complete Rust-owned `aria2-core` test target set passed with all features
enabled. This includes the core library, HTTP/FTP/SFTP/BitTorrent/Metalink/DHT
and tracker integration targets, session and control-file persistence, retry,
pause/remove/unpause lifecycle, concurrent downloads, checksum recovery,
stress, disk-I/O, and performance regression targets. No test failure was
observed; ignored tests remain explicitly reported by their individual target
and are not counted as passing evidence.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --tests -- --test-threads=1
  exit_code=0
  library: 3440 passed, 0 failed, 1 ignored
  all integration, E2E, stress, and performance targets: passed
~~~

This closes the broad local Rust-owned core regression and lifecycle evidence
slice only. It does not prove live third-party services, original-client or
browser interoperability, platform-specific behavior, bindings, or final
workspace acceptance. The active phase remains `phase-2-core-domain`
(`in_progress`) and the migration remains `PARTIAL`.

## 2026-08-17 Removed Status Predicate Checkpoint

`DownloadStatus::is_stopped()` now includes `Removed`, matching the core
stopped-result store and RPC contract where completed, failed, and removed
downloads are all terminal stopped results. Active and waiting statuses remain
the only non-stopped states.

Rust-owned verification:

~~~text
cargo test -p aria2-rpc --all-features --lib types::tests::test_download_status_variants -- --exact --test-threads=1
  1 passed, 0 failed
cargo fmt --all -- --check
  PASS
~~~

This closes the status-predicate consistency boundary only. The active phase
remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; broader lifecycle combinations, protocol interoperability,
bindings, performance, and final workspace acceptance remain open.

## 2026-08-17 Integrity Digest-Count Boundary Checkpoint

Single-file and multi-file piece-hash validators now reject a non-empty digest
list whose length does not equal the logical piece count. Previously, a short
list could make validation finish early and report success without checking the
remaining payload. Missing physical multi-file payloads still skip the
pre-download validator so the owning BitTorrent command can recover them
through its normal piece-download path.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib checksum::check_integrity -- --test-threads=1
  60 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_checksum -- --test-threads=1
  13 passed, 0 failed
cargo test -p aria2-core --features bittorrent --test test_e2e_bittorrent_download integrity -- --test-threads=1
  4 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the malformed piece-digest-count boundary only. The active phase
remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; legacy integrity-wrapper ownership, broader lifecycle combinations,
live protocol interoperability, bindings, performance, and final workspace
acceptance remain open.

## 2026-08-17 RequestGroup Pause/Success Race Checkpoint

The engine completion state machine now preserves `Paused` when a group's final
command reports success after a pause request. A clean completion still becomes
terminal when no pause is active, and user-removal or timeout halt reasons keep
their stronger terminal precedence. This prevents a pause/completion race from
turning a resumable task into a finished stopped result.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib engine::engine_loop::tests -- --test-threads=1
  16 passed, 0 failed
cargo test -p aria2-core --all-features --lib request::request_group -- --test-threads=1
  116 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the final-command pause/completion race only. The active phase
remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; broader cross-protocol lifecycle, interoperability, bindings,
performance, and final workspace acceptance remain open.

## 2026-08-17 Write-Back Cache Ordering Checkpoint

`CachedDiskWriter` now flushes pending small cached ranges before a large
write bypasses the write-back cache. Without this ordering barrier, a later
cache flush could overwrite newer direct-write bytes with stale cached data.
The same rule is applied to both slice-based and `Bytes`-based positioned
writes. The original `SegmentMan` per-piece flush rationale was audited: the
active Rust BitTorrent path owns one positioned writer and its cache ordering,
while the public `SegmentMan` facade does not own a disk writer or piece-local
cache. Its stale cache TODOs were removed rather than creating a second cache
owner.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib filesystem::disk_writer -- --test-threads=1
  22 passed, 0 failed
cargo test -p aria2-core --all-features --lib filesystem::disk_cache -- --test-threads=1
  20 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download -- --test-threads=1
  30 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the direct-write ordering boundary only. Cache read-through
acceleration, broader production aggregation/error propagation, live protocol
interoperability, and final workspace acceptance remain open; the active phase
remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`.

## 2026-08-17 Piece-Storage Bitfield Boundary Checkpoint

The piece-storage boundary now matches the original `BitfieldMan::setBitfield`
contract for invalid input: an empty or mismatched byte buffer is rejected
without changing the completion bitfield or in-use state. `DefaultPieceStorage`
also returns before updating piece statistics or resetting selector state for
the same invalid input. Valid bitfields retain the original behavior of
replacing completion state and clearing in-use bits.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib segment::piece_storage -- --test-threads=1
  75 passed, 0 failed
~~~

This closes the invalid-bitfield storage boundary only. The migration remains
`PARTIAL`; broader lifecycle combinations, protocol interoperability, bindings,
performance, and final workspace acceptance remain open.

## 2026-08-17 Multi-File Integrity Missing-Payload Checkpoint

BitTorrent multi-file integrity task creation now treats a missing non-empty
physical payload file as incomplete data, matching the single-file path. The
helper returns no pre-download integrity task so the owning command enters its
normal piece-download path instead of surfacing a terminal I/O error while
trying to hash a file that is not present. Zero-length entries remain valid
without a physical file.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib checksum::check_integrity -- --test-threads=1
  58 passed, 0 failed
~~~

This closes the missing multi-file payload dispatch slice only. The migration
remains `PARTIAL`; broader lifecycle combinations, protocol interoperability,
bindings, performance, and final workspace acceptance remain open.

## 2026-08-17 Control-File Piece-Length Checkpoint

Rust-owned control files now restore and normalize the caller's logical piece
count when opened. The serialized bitfield is resized to that logical count,
unused bits in its final byte are cleared, and persisted progress is recomputed
from the normalized pieces instead of trusting a count from the old layout.
Completion accounting sums each set piece using a bounded ceiling piece length,
so a short final piece and non-byte-aligned piece counts produce the correct
persisted progress. Invalid trailing piece indexes are ignored.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib filesystem::control_file::tests --all-features -- --test-threads=1
  13 passed, 0 failed
cargo test -p aria2-core --all-features --lib filesystem::resume_helper -- --test-threads=1
  14 passed, 0 failed
cargo test -p aria2-core --test test_e2e_disk_io --all-features -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test test_e2e_download test_e2e_engine_sequential_http_ --all-features -- --test-threads=1
  2 passed, 0 failed
cargo test -p aria2-core --test test_e2e_concurrent_http_range test_multi_mirror_resume_restores_completed_segments --all-features -- --test-threads=1
  1 passed, 0 failed
cargo fmt --all -- --check
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
git diff --check
  PASS
~~~

This closes the control-file piece-count and short-final-piece accounting
slice only. The migration remains `PARTIAL`; broader lifecycle combinations,
protocol interoperability, bindings, performance, and final workspace
acceptance remain open.

## 2026-08-17 RequestGroup Halt/Pause Flag Checkpoint

RequestGroup halt transitions now clear both graceful and forced pause flags.
This preserves halt precedence without leaving an impossible forced-pause state
behind for later lifecycle inspection or promotion. The change is internal to
the Rust control-flag model and does not alter public status, result-code, or
RPC wire values.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib request::request_group::halt_reason::tests -- --test-threads=1
  9 passed, 0 failed
cargo test -p aria2-core --all-features --lib request::request_group::tests -- --test-threads=1
  35 passed, 0 failed
cargo test -p aria2-core --all-features --lib request::request_group_man -- --test-threads=1
  36 passed, 0 failed
cargo fmt --all -- --check
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
git diff --check
  PASS
~~~

This closes the halt/pause flag invariant only. The active phase remains
`phase-2-core-domain` (`in_progress`) and the migration remains `PARTIAL`;
broader cross-protocol lifecycle, interoperability, bindings, performance, and
final workspace acceptance remain open.

## 2026-08-17 RequestGroup Command-Counter Checkpoint

The RequestGroup command counter now saturates at zero when an unbalanced
completion attempts to decrement an empty counter. Valid decrements retain the
previous-count return value used by engine demotion, while duplicate or stale
completion handling cannot wrap the counter to `u32::MAX` and strand a group in
the active store.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib request::request_group::tests -- --test-threads=1
  36 passed, 0 failed
cargo test -p aria2-core --all-features --lib engine::engine_loop::tests -- --test-threads=1
  15 passed, 0 failed
cargo test -p aria2-core --lib --all-features -- --test-threads=1
  3438 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the command-counter underflow boundary only. The active phase
remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; broader lifecycle, protocol, interoperability, bindings,
performance, and final workspace acceptance remain open.

## 2026-08-16 BitTorrent Save-Session Checkpoint

The BitTorrent checkpoint owner now treats an explicit session-save request as
a durable boundary. After a verified peer or web-seed piece is written, the
owner flushes its positioned/cache-backed writer before saving the matching
piece bitfield and consumes the request only after both operations succeed.
This keeps a sidecar-marked piece readable after the save boundary instead of
allowing the in-memory write cache to get ahead of the checkpoint.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download test_e2e_bt_save_session_flushes_requested_checkpoint -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download -- --test-threads=1
  30 passed, 0 failed, 2 ignored
~~~

This closes the explicit BitTorrent save-session checkpoint and writer-flush
slice only. Failure-retry behavior, broader cross-protocol lifecycle coverage,
third-party interoperability, and final workspace acceptance remain open; the
active phase remains `phase-2-core-domain` (`in_progress`) and the overall
status remains `PARTIAL`.

## 2026-08-16 Concurrent HTTP Save-Session Checkpoint

The single-mirror concurrent HTTP owner now shares the explicit control-file
flush helper with the multi-mirror owner. The helper is reached at startup,
write, segment-completion, and cancellation-timer boundaries; it flushes the
writer, updates the committed segment progress, saves the `.aria2` sidecar,
and consumes the request only after the save succeeds. The regression waits
for committed sidecar progress rather than transient in-flight progress before
invoking the real `SaveSessionCommand` path.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_concurrent_http_range test_concurrent_save_session_flushes_requested_control_file -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_concurrent_http_range -- --test-threads=1
  9 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes the explicit concurrent HTTP save-session owner slice only.
Broader lifecycle combinations, protocol interoperability, and final
workspace acceptance remain open; the active phase remains
`phase-2-core-domain` (`in_progress`) and the overall status remains `PARTIAL`.

## 2026-08-16 Stopped-Result GID Uniqueness Checkpoint

The stopped-result store now enforces the original `IndexedList` contract that
each terminal result is keyed uniquely by GID. A replayed or racing lifecycle
path is rejected without changing the first result or its FIFO position, so
`tellStopped`, `getDownloadResult`, removal, and pruning cannot expose duplicate
terminal entries for one task.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --lib request::request_group_man::stopped -- --test-threads=1
  3 passed, 0 failed
cargo test -p aria2-core --all-features --lib request::request_group_man -- --test-threads=1
  36 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes stopped-result key uniqueness only. The migration remains
`PARTIAL`; broader lifecycle, protocol interoperability, bindings, performance,
and final workspace acceptance are still open.

## 2026-08-16 Retry And Cross-Protocol Fixture Audit

The local retry audit found no implementation regression in the existing
phase-2 paths. Concurrent HTTP Range retries and fallback, sequential gap
retries with partial progress, FTP terminal protocol failures, and SFTP
not-found/checksum/resume lifecycle cases all pass their Rust-owned fixtures.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_concurrent_http_range -- --test-threads=1
  9 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_gap_retry -- --test-threads=1
  26 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --test test_e2e_ftp_download -- --test-threads=1
  36 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --test test_e2e_sftp_download -- --test-threads=1
  23 passed, 0 failed, 2 ignored
~~~

This closes only the local retry-fixture audit. Third-party servers,
original-client interoperability, ignored network cases, broader protocol
combinations, and final workspace acceptance remain open; the migration stays
`PARTIAL`.

### Phase 1 Evidence

- `aria2_original/configure.ac:5` identifies the reference release as `1.37.0`.
  Its `src/` tree contains 415 `.cc` files and 523 C/C++ headers. The
  historical 530-unit ledger excludes the public API header
  `src/includes/aria2/aria2.h`: the remaining 115 headers have no matching
  `.cc`, so 415 + 115 = 530. That public header is tracked separately by the
  C API row below; the source-count discrepancy is resolved.
- The Rust workspace metadata currently contains `aria2`, `aria2-core`,
  `aria2-protocol`, and `aria2-rpc`, with their feature sets recorded by
  `cargo metadata --no-deps --format-version 1`. The 20 module records under
  `docs/migration/` and the current matrix are present. The matrix now also
  owns tests/fixtures, bindings/SDKs, examples/benchmarks, and workspace/CI;
  the phase-1 matrix and validation plan are complete; the next phase is
  constrained to the core domain evidence below and may not be skipped.
- The targeted test-boundary search found `aria2_original` only in comments,
  documentation strings, and assertion text in Rust sources. It found no
  `include_str!`/`include_bytes!` source loading or process invocation that
  reads, builds, links, starts, or dynamically depends on the reference tree.
- Current workspace evidence: `cargo test -p aria2-rpc --test
  test_rpc_shutdown --all-features -- --test-threads=1` passed 5 tests;
  `test_e2e_dynamic_rate_limit` passed 2 tests;
  `test_rpc_system_methods` passed 7 tests; and
  `cargo test -p aria2-core --all-targets --no-run` passed. These validate the
  current RPC test-boundary change only and do not close a migration phase.

## 2026-08-16 Core HTTP lifecycle checkpoint

The engine-level sequential HTTP regression now covers the complete
pause/unpause and removal lifecycle through `DownloadEngine`: pausing a live
stream saves a non-empty partial control file, unpause allows the group to be
re-promoted and finish, and removal retains both the partial output and its
control file. The pause fixture intentionally ignores Range requests, so the
test sets `always-resume=false` to exercise the explicit fresh-download
fallback after the saved checkpoint is observed. This is local lifecycle
evidence, not range-resume interoperability evidence.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_download test_e2e_engine_sequential_http -- --test-threads=1
  2 passed, 0 failed
~~~

The workspace all-targets gate was also retried. Two host-sensitive tests
flaked in separate runs (`client_identity` mutual TLS connection reset and the
performance stability CV threshold); each passed when rerun in isolation. A
final workspace attempt was blocked by Windows pagefile exhaustion while
mapping `libaria2_core.rlib` (`os error 1455`). No test was weakened or changed
to hide these environment conditions.

This closes only the local sequential HTTP engine pause/unpause/removal slice.
Third-party HTTP range behavior, broader cross-protocol lifecycle coverage,
owner-side integrity-plan application, interoperability, and final workspace
acceptance remain open; the migration remains `PARTIAL`.

## 2026-08-16 RequestGroup Lifecycle Transition Checkpoint

`RequestGroupMan::force_remove_group` now participates in the same lifecycle
lock as promotion and requeue. Previously, force removal could inspect the
active and reserved stores while the engine was moving a group between them,
so it could observe an intermediate store state. The manager now serializes
group addition, promotion, requeue, demotion, and removal transitions while
retaining the canonical GID index for concurrent lookups.

The regression holds the lifecycle lock across a reserved-to-active transition,
starts force removal on another thread, and asserts that the call waits until
the transition lock is released before publishing the force-halt request. This
is manager-level lifecycle evidence; it does not close broader pause/resume,
retry, storage, control-file, protocol, or interoperability gaps.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --lib request::request_group_man -- --test-threads=1
  34 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the force-removal transition-race slice only. The active phase
remains `phase-2-core-domain` (`in_progress`), the overall status remains
`PARTIAL`, and broader lifecycle, cross-protocol interoperability, bindings,
performance, and final workspace acceptance remain open.

## 2026-08-16 Sequential HTTP Save-Session Control-File Checkpoint

The existing production `SaveSessionCommand` path requests a checkpoint on the
shared `RequestGroupMan`. The active sequential HTTP owner then flushes its
disk writer, updates and saves the `.aria2` sidecar, and consumes the request
only after the save path succeeds.

The Rust-owned regression runs the real `DownloadEngine` against the slow HTTP
fixture, invokes `SaveSessionCommand` with the same manager used by the active
download, and verifies both a nonzero persisted checkpoint length and a cleared
save request. It then removes the task and confirms the partial checkpoint is
retained during terminal cleanup.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_download test_e2e_engine_save_session_flushes_sequential_http_control_file -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_download -- --test-threads=1
  36 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the core sequential HTTP save-session owner path only. RPC HTTP
wire invocation, concurrent/multi-mirror and other protocol owners, stopped
task deduplication, live third-party interoperability, and final workspace
acceptance remain open; the overall status remains `PARTIAL`.

## 2026-08-16 Integrity Callback Dispatch Plan Checkpoint

The legacy `StreamCheckIntegrity` and `BtCheckIntegrity` wrappers now return
explicit Rust-owned dispatch plans for incomplete checks, successful BT checks,
and trailing-garbage cleanup. Plans carry the physical file paths and declared
lengths, reset-piece-storage intent, hash-check-only gating, seed gating, and
completion-hook intent. The BT incomplete branch now reflects the original
behavior by continuing to file allocation unless hash-check-only is enabled.

The plans are deliberately values rather than hidden side effects: mutable
`PieceStorage` access and async allocation/truncation remain with the owning
command. Production downloads already execute those operations through
`CheckIntegrityTask`/`IntegrityOutcome` and the existing async managers; these
legacy wrappers still have no production callers, so the owner-side application
of their plans remains a separate compatibility gap.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --lib checksum::check_integrity -- --test-threads=1
  56 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo test --workspace --all-targets --all-features --quiet
  PASS
~~~

This closes the callback-plan interface and regression slice only. The
integrity matrix remains `PARTIAL`; owner-side application evidence, broader
protocol lifecycle coverage, live third-party interoperability, and final
workspace acceptance remain open.

## 2026-08-16 Integrity Owner Application Follow-up

The production HTTP command now applies the shared trailing-garbage plan before
its piece-hash validation. The BitTorrent command applies the same plan for
single- and multi-file payloads; on a complete hash check, the shared BT
success plan now supplies both the completion-hook decision and the file list
consumed by the existing allocation manager. These are Rust-native owner calls
and do not add a C++ callback hierarchy or change public options and wire data.

The pre-existing `RequestGroupMan::lifecycle_lock` worktree change is now used
to serialize add, promote, requeue, and remove transitions across the canonical
group index and scheduling stores. This prevents lifecycle calls from
observing an intermediate store transfer while preserving concurrent lookups.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --lib checksum::check_integrity -- --test-threads=1
  57 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_download test_e2e_http_check_integrity_applies_trailing_cleanup_plan -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --features bittorrent --test test_e2e_bittorrent_download integrity -- --test-threads=1
  4 passed, 0 failed
cargo test -p aria2-core --all-features --lib request::request_group_man -- --test-threads=1
  33 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

The incomplete action still requires a mutable `PieceStorage` owner that the
current HTTP and BT commands do not retain; that wrapper-only branch remains
`PARTIAL`. Live third-party interoperability, broader cross-protocol lifecycle
coverage, and final workspace acceptance remain open.

## 2026-08-16 Adaptive HTTP Range Capacity Retry Checkpoint

HTTP 429 responses from segmented Range requests are now mapped to typed
`ServerError` values. The adaptive controller therefore closes the capacity
round, lowers the per-authority target, and requeues the affected ranges
without consuming ordinary segment retry attempts. The regression asserts the
expected retry counts and an exact output match.

The affected segmented HTTP fixtures now preserve `min-split-size` in each task snapshot.
The adaptive fixture uses an 8 MiB payload with a Rust-owned `1M` snapshot, so
the 429, multi-mirror, shared-authority, split-budget, and cancellation cases
exercise valid segmented ranges instead of relying on an implicit default.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_concurrent_http_range -- --test-threads=1
  8 passed, 0 failed
cargo test -p aria2-core --test test_http_adaptive_concurrency_e2e -- --test-threads=1
  5 passed, 0 failed
cargo test -p aria2-core --lib request::request_group -- --test-threads=1
  99 passed, 0 failed
cargo test -p aria2-core --lib session::session_serializer -- --test-threads=1
  8 passed, 0 failed
cargo test -p aria2-core --all-features --lib engine::http_segment_downloader -- --test-threads=1
  24 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_rate_limit -- --test-threads=1
  9 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_http_concurrent -- --test-threads=1
  9 passed, 0 failed
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3427 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes only the adaptive segmented HTTP capacity-retry slice. The active
phase remains `phase-2-core-domain` (`in_progress`) and the overall status
remains `PARTIAL`; broader lifecycle, interoperability, bindings, performance,
and final workspace gates are still open.

### Worktree Ownership Record

The following pre-existing worktree changes are in scope for the next audit
and must be preserved: release publishing changes in
`.github/workflows/release.yml`; dependency and lockfile changes in
`Cargo.lock` and `aria2-core/Cargo.toml`; option-registry documentation in
`aria2-core/src/config/option_definitions/mod.rs`; the RPC test relocation and
new fixtures under `aria2-rpc/tests/`; the CLI compatibility fixture and test;
the Python package metadata; and the changes already present in
`docs/MIGRATION.md` and this file. The deleted
`aria2-core/tests/test_rpc_shutdown.rs` is replaced by the RPC-owned test
recorded above. No unrelated change may be reverted or overwritten.

### Completed Improvements And Next Action

- The compatibility inventory is Rust-owned for tests, the short-option
  extension boundary is documented separately, and the current matrix records
  behavior evidence instead of treating source comparison as compatibility.
- Phase 1 verification passed: `cargo fmt --all -- --check`,
  `cargo test --workspace --all-targets --no-run`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and
  `git diff --check` all exited successfully on 2026-08-15. The worktree
  record and Rust-owned test-boundary audit also passed, so the phase is now
  `passed_locked`.
- The shared `RetryPolicy` now retries only timeouts, temporary network
  failures, and `ServerError` codes present in its configured HTTP allowlist.
  404, resume, authentication, redirect, range, and other protocol failures
  are terminal at this policy seam; the public `max-tries` total-attempt and
  unlimited-zero semantics remain unchanged.
- Buffered and streaming segmented HTTP Range requests now share structured
  status classification: 416 is `RangeNotSatisfiable`, 429 and 5xx are
  `ServerError`, authentication statuses remain auth failures, ordinary 4xx
  responses such as 403 are terminal `HttpProtocolError` values, and 404 is a
  typed `ResourceNotFound` value rather than `FatalError::Config`.
- The single-URI adaptive HTTP executor now treats 429 as capacity feedback:
  it reduces admission, preserves the ordinary segment retry budget, and
  requeues the affected ranges. Rust-owned E2E coverage proves that a
  rate-limited download cannot report success with preallocated gaps left
  unwritten.
- `max-file-not-found` is now enforced per RequestGroup across sequential
  HTTP, segmented HTTP, and in-memory metadata downloads. A configured limit
  produces `MaxFileNotFound`; zero keeps the first 404 terminal as
  `ResourceNotFound`, and the shared retry policy still treats generic 404s as
  non-retryable.
- FTP 550 responses from directory traversal, `SIZE`, and `RETR` now use the
  same typed `ResourceNotFound` result and RequestGroup counter. The FTP
  command retry loop applies both `max-file-not-found` and total-attempt
  limits; SFTP `SSH_FX_NO_SUCH_FILE` maps to the same result and bounded
  not-found retry path. Permission and transport failures retain their
  separate error classes.
- Metalink direct mirrors and torrent metaurl fallback loops now apply the
  owning RequestGroup's not-found counter. A configured terminal threshold
  returns `MaxFileNotFound` before further mirror failover, while transport,
  server, and other protocol failures retain their existing fallback paths.
- Sequential HTTP and in-memory metadata cancellation now interrupts pending
  body reads and preserves the existing partial-checkpoint rules; autosave
  requests now exclude terminal RequestGroups; concurrent-to-sequential gap
  recovery now observes the same cancellation boundary. The immediate next
  action is to audit the remaining phase-2 RequestGroup pause/resume, retry,
  storage, and checksum gaps with Rust-owned E2E evidence, then expand live
  protocol interoperability and broader cross-protocol lifecycle E2E before
  any later phase is opened.
- Stopped-result storage now rejects duplicate GIDs, matching the original
  keyed `IndexedList` contract while preserving the first result and FIFO
  order. The focused store and manager lifecycle tests pass; broader terminal
  lifecycle and cross-protocol coverage remain open.
- Piece-storage bitfield loading now rejects empty and mismatched buffers
  without mutating completion or in-use state, matching the original
  `BitfieldMan::setBitfield` boundary. The focused storage suite passes; broader
  lifecycle and checkpoint combinations remain open.
- Multi-file BitTorrent integrity checks now skip pre-validation when a
  non-empty physical payload file is absent, allowing the normal piece download
  path to recover it instead of returning an integrity I/O error. The focused
  checksum suite covers this dispatch boundary; broader multi-file lifecycle
  combinations remain open.
- Control-file reloads now restore the logical piece count supplied by the
  current download, normalize stale serialized bits, and calculate completed
  bytes piece-by-piece, including a bounded short final piece. Focused
  control-file tests cover this; broader cross-process resume combinations
  remain open.
- RequestGroup halt transitions now clear both graceful and forced pause flags,
  preserving halt precedence without stale pause intent. Focused flag and
  manager lifecycle tests cover this; broader cross-protocol lifecycle evidence
  remains open.
- RequestGroup command-counter decrements now saturate at zero, preserving the
  previous-count contract without allowing duplicate completion paths to wrap
  the counter. Focused RequestGroup and engine-loop tests cover this; broader
  lifecycle and workspace evidence remains open.
- The engine completion state machine now keeps a paused RequestGroup paused
  when its final command reports success after a pause request. User-removal
  and timeout halt reasons retain terminal precedence. Focused engine-loop and
  RequestGroup suites cover this race; broader cross-protocol lifecycle
  evidence remains open.
- Single-file and multi-file integrity validators now reject non-empty piece
  digest lists whose count differs from the logical piece count, preventing an
  early validator finish from accepting unchecked payload bytes. Focused core
  and integrity E2E suites cover the boundary; legacy wrapper ownership and
  broader integrity lifecycle combinations remain open.

## 2026-08-16 FTP/SFTP Not-Found E2E Checkpoint

The Rust-owned protocol fixtures now exercise the typed remote not-found
result through the real command loops. FTP `550` responses and SFTP
`SSH_FX_NO_SUCH_FILE` responses return `ResourceNotFound` with the default
threshold, while an effective `max-file-not-found=2` snapshot terminates each
loop with `MaxFileNotFound` after the second remote failure.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_ftp_download -- --test-threads=1
  36 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --test test_e2e_sftp_download -- --test-threads=1
  23 passed, 0 failed, 2 ignored
~~~

This closes the local FTP/SFTP not-found lifecycle slice only. Third-party
FTP/FTPS/SFTP servers, original-client interoperability, broader cross-
protocol lifecycle combinations, and final workspace acceptance remain open;
the overall status remains `PARTIAL`.

## 2026-08-16 Follow-mode And Session Graph Variant Checkpoint

The session persistence path now has Rust-owned regression coverage for all
three explicit follow values: `true`, `false`, and `mem`. The values survive
the complete `RequestGroup -> ResumeData -> RequestGroup` round trip as typed
`FollowMode` variants. The post-download handler chain also verifies that
`false` disables the corresponding handlers while `mem` remains an enabled
follow mode. Existing Metalink graph reconstruction and generated-child
exclusion tests remain green.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib session::session_persistence::tests -- --test-threads=1
  19 passed, 0 failed
cargo test -p aria2-core --all-features --lib engine::post_download_handler::tests::test_build_handler_chain_respects_follow_modes -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_session -- --test-threads=1
  13 passed, 0 failed
cargo test -p aria2-core --all-features --test deep_e2e_cross_protocol -- --test-threads=1
  18 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --test test_e2e_checksum -- --test-threads=1
  13 passed, 0 failed
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3419 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the typed follow-mode persistence and handler-selection slice
only. Integrity-dispatch callback branches, live third-party protocol
interoperability, broader lifecycle combinations, and final workspace
acceptance remain open; the overall status remains `PARTIAL`.

## 2026-08-16 Segmented HTTP Range Status Classification Checkpoint

The buffered and streaming segmented HTTP Range paths now share one structured
status classifier. HTTP 416 remains `RangeNotSatisfiable`, HTTP 5xx remains
`ServerError`, 401/407 remains an authentication failure, and ordinary 4xx
responses such as 403 remain terminal `HttpProtocolError` values while 404
returns typed `ResourceNotFound` instead of `FatalError::Config`. This keeps
the Range seam consistent with the sequential and gap download paths and
preserves the public not-found result code.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib engine::http_segment_downloader -- --test-threads=1
  24 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_gap_retry -- --test-threads=1
  26 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

The focused library tests include local HTTP fixtures for buffered 404 and
streaming 403 responses, plus direct 404/503 classifier coverage. The
segmented Range path now feeds the RequestGroup not-found counter; broader
protocol lifecycle combinations and original-client interoperability remain
open phase-2 work, so the overall status remains `PARTIAL`.

## 2026-08-16 `max-file-not-found` Lifecycle Checkpoint

The HTTP not-found path now has one RequestGroup-owned counter and typed
classification. Sequential HTTP, sequential gap/auth-retry paths, concurrent
single- and multi-mirror Range paths, and in-memory metadata downloads use the
same effective task option snapshot. `max-file-not-found=0` returns
`ResourceNotFound` without retrying; a positive limit permits 404 retries and
returns `MaxFileNotFound` at the configured terminal count. Generic
`RetryPolicy` 404 behavior remains terminal, so unrelated HTTP protocol errors
are not broadened into retries.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --lib
  3415 passed, 0 failed, 1 ignored
cargo test -p aria2-core --all-features --test test_e2e_download -- --test-threads=1
  32 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --test test_e2e_download max_file_not_found -- --test-threads=1
  3 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

The focused E2E fixture verifies sequential, concurrent segmented, and
in-memory 404 failures preserve `MaxFileNotFound`; the sequential fixture
observes exactly three requests for a configured limit of three. This closes
the current not-found classification and retry slice only. FTP/SFTP status
parity, broader cross-protocol lifecycle combinations, original-client
interoperability, and final workspace acceptance remain open; the overall
status remains `PARTIAL`.

## 2026-08-16 FTP/SFTP Remote Not-Found Classification Checkpoint

FTP 550 responses from CWD traversal, `SIZE`, and `RETR` now return typed
`ResourceNotFound` errors instead of falling through as unknown fatal errors.
The FTP command records each response in the owning RequestGroup, returns
`MaxFileNotFound` at the configured zero-progress threshold, and still obeys
the public total-attempt `max-tries` limit. SFTP `SSH_FX_NO_SUCH_FILE` errors
from remote file operations use the same result code and bounded
`max-file-not-found` retry behavior. Permission-denied and transport errors
remain distinct.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --lib engine::ftp_download_command::tests -- --test-threads=1
  22 passed, 0 failed
cargo test -p aria2-core --all-features --lib engine::sftp_download_command::tests -- --test-threads=1
  16 passed, 0 failed
cargo test -p aria2-core --all-features --lib
  3416 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the typed remote-not-found command seam only. No live third-party
FTP/SFTP server fixture was added in this checkpoint, and broader protocol
status matrices, cross-protocol lifecycle E2E, and original-client
interoperability remain open; the overall status remains `PARTIAL`.

## 2026-08-16 Stopped Result Follow-graph Checkpoint

Completed groups now regenerate their `DownloadResult` after post-download
processing creates follow-up child groups. Previously, the demotion scan
captured the result before `followed_by` child GIDs were attached to the live
parent, so `tellStopped` and other stopped-result consumers could lose that
relationship. Error and removed groups retain their original demotion snapshot.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib request::request_group_man --all-features -- --test-threads=1
  33 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

The manager regression uses an in-memory Metalink fixture and verifies that a
completed stopped parent exposes its generated child GID while the child is
queued for promotion. This closes only the stopped-result relationship slice;
broader follow/session graph variants, cross-protocol lifecycle E2E, and
original-client interoperability remain open phase-2 work. The overall status
remains `PARTIAL`.

## 2026-08-16 Metalink Not-Found Retry Checkpoint

Metalink owns its mirror and torrent-metaurl loops, so its HTTP 404 responses
do not pass through the ordinary HTTP response command. Those loops now route
`ResourceNotFound` through the owning RequestGroup counter and stop with
`MaxFileNotFound` at the configured zero-progress threshold. Other errors keep
their existing mirror and metaurl failover behavior.

Rust-owned verification on 2026-08-16:

~~~text
cargo test -p aria2-core --all-features --lib engine::metalink_download_command -- --test-threads=1
  20 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

The focused localhost fixture serves two 404 mirrors and verifies that the
second response produces `MaxFileNotFound` after exactly two requests. This
closes the Metalink not-found counter seam only; session/follow graph variants,
integrity-dispatch interface cleanup, live protocol interoperability, and
cross-protocol lifecycle E2E remain open, so the overall status remains
`PARTIAL`.

### Phase 1 Validation Plan

| Gate | Required Rust-owned evidence |
| --- | --- |
| Reference inventory | `aria2_original/configure.ac`, the 415 `.cc` plus 115 implementation header-only units, the separately owned public C API header, source/build/test directory inventory, and the 20 detailed module records |
| Rust ownership inventory | `cargo metadata --no-deps --format-version 1`, all four crate feature sets, public C/Python/Node surfaces, four examples, benchmark targets, and both CI workflows each mapped to one current matrix row |
| Compatibility boundary | Rust-owned CLI/config/RPC/protocol fixtures and baselines; a source audit proving no test reads, builds, links, starts, or dynamically loads `aria2_original` |
| Lifecycle and protocol E2E plan | Local fixtures for HTTP/HTTPS, FTP, SFTP, BitTorrent/DHT, Metalink, proxy/auth, RPC/JSON-RPC/XML-RPC/WebSocket, retry/resume, concurrency, checksum, cancellation, pause/resume, removal, restart, and abnormal-network paths |
| Public-surface plan | CLI/config defaults and aliases, session/control files, notifications/errors, C API, Python and Node SDK tests, examples, and browser/original-client contract tests with Rust-owned expected results |
| Performance plan | Reproducible Rust workloads for throughput, latency, CPU, memory, allocations, I/O, lock contention, and concurrency; comparable `aria2_original` measurements are independent audit evidence and never test runtime inputs |
| Phase exit | `cargo fmt --all -- --check`, relevant Clippy with `-D warnings`, focused Rust tests, `git diff --check`, and a complete worktree/matrix ownership review; passed on 2026-08-15 and locked as `phase-1-baseline-matrix` |

### Remaining Risks And Reopen Conditions

The current matrix remains `PARTIAL` for protocol semantics, original-client
and browser-extension interoperability, platform coverage, public C ABI
compatibility, measured C/Rust performance comparison, and several lifecycle
and E2E slices. Reopen phase-1 review if the reference-tree inventory changes,
the matrix ownership or status rules change, a test begins loading external
reference data, or any recorded worktree path is added, removed, or moved.

## 2026-08-14 Independent Product Identity Guard

The product identity boundary is covered at both public entry points. The CLI
`-v` action and RPC `aria2.getVersion.version` resolve to
`aria2_protocol::identity::PRODUCT_VERSION`, while the CLI output must not
reintroduce the upstream C++ version-report text. This check changes no option
definition, default value, config-file behavior, or user configuration.

Verification on 2026-08-14:

~~~text
cargo test -p aria2 --all-features --test test_cli_options --test test_rpc_api_compatibility -- --test-threads=1
  105 passed, 0 failed
   64 passed, 0 failed
~~~

This closes only the product-identity regression surface. It does not change
the remaining `PARTIAL` status for original-client interoperability, complete
RPC/browser-extension coverage, or workspace end-to-end acceptance.

## 2026-08-14 Workspace and SDK Acceptance Checkpoint

The workspace now compiles all Rust test and benchmark targets in one pass,
and both maintained SDK suites pass on this host. Python uses the bundled
Python 3.12 runtime plus the ignored `.codex-python-deps` test environment;
no package manifest, lockfile, or project configuration was changed.

~~~text
cargo test --workspace --all-targets --no-run
  PASS
npm test -- --run (bindings/nodejs)
  123 passed, 0 failed
python -m pytest -p no:cacheprovider (bindings/python)
  137 passed, 0 failed
~~~

This closes the current SDK and workspace compilation slice. It does not claim
that one aggregate workspace test execution, platform-specific binding runs,
public C ABI compatibility, or complete original-client/browser-extension
interoperability is complete.

## 2026-08-15 Full Workspace and Internal Seam Checkpoint

The full Rust workspace test-and-benchmark target run now has an explicit
successful process exit after the failure-path test timing correction. The
test helper uses a one-second retry wait, matching the production option's
seconds unit; no product default or user configuration was changed.

~~~text
cargo test --workspace --all-targets
  exit_code=0
  5632 passed, 0 failed, 47 ignored
cargo fmt --all -- --check
  PASS
cargo clippy --workspace --all-targets -- -D warnings
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS after internal cleanup
npm test (bindings/nodejs)
  123 passed, 0 failed
npm run typecheck (bindings/nodejs)
  PASS
python -m pytest -p no:cacheprovider (bindings/python, bundled Python 3.12)
  137 passed, 0 failed
~~~

The original source tree has no NTLM HTTP authentication implementation. The
Rust-only `NtlmState` API was therefore removed instead of retaining a public
stub that always returned an error. Basic and Digest authentication remain
separate supported compatibility paths. This is internal dead-code cleanup;
it does not alter CLI/configuration, RPC, wire formats, product identity, or
the required original-client surface.

This checkpoint strengthens the reproducible test and ownership evidence but
does not change the overall `PARTIAL` status. Original-client interoperability,
platform-specific runs, public C ABI compatibility, and the remaining module
matrix gaps still require independent evidence.

The checksum seam audit for this checkpoint found that production download
commands use `check_integrity::man::CheckIntegrityTask` and
`IntegrityOutcome`; the exported `StreamCheckIntegrity`, `BtCheckIntegrity`,
`CheckIntegrityKind`, and `ValidatorKind` types have no production callers and
retain unconnected lifecycle methods. They remain an explicit Rust-crate
interface decision rather than being silently deleted during this migration.

~~~text
cargo test -p aria2-core --all-features --lib checksum::check_integrity -- --test-threads=1
  52 passed, 0 failed
~~~

The all-features workspace target run was also repeated after the fixture and
documentation boundary cleanup:

~~~text
cargo test --workspace --all-targets --all-features --quiet
  exit_code=0
  aria2 CLI regression: 109 passed, 0 failed
  aria2-core all-features library: 3397 passed, 0 failed, 1 ignored
  aria2-rpc all-method E2E: 55 passed, 0 failed
~~~

The command-level exit is the acceptance evidence; the listed target counts
are representative checks from the same run, not an aggregate count. This
still does not close original-client interoperability, platform coverage, or
the remaining `PARTIAL` module matrix.

## 2026-08-15 Retry Error Classification Checkpoint

`RetryPolicy::should_retry` no longer treats every `RecoverableError` as
retryable. The shared policy now admits only timeouts, temporary network
failures, and configured `retryable_http_codes`; HTTP 404, `CannotResume`,
authentication failures, redirect failures, range failures, and other
protocol errors stop without another attempt. This preserves the existing
`max-tries` total-attempt contract and `max-tries=0` unlimited behavior.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib engine::retry_policy --all-features -- --test-threads=1
  18 passed, 0 failed
cargo test -p aria2-core --test test_retry --all-features -- --test-threads=1
  16 passed, 0 failed
cargo test -p aria2-core --test test_error_network --all-features -- --test-threads=1
  32 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test test_e2e_gap_retry --all-features -- --test-threads=1
  26 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

The HTTP fixture counts requests: a persistent 500 uses two total attempts
when configured for two, while a persistent 404 uses one request even when
the policy allows three total attempts. The separate skip-response
`max-file-not-found` classification and the broader inconsistent 4xx mapping
across range, Metalink, and gap paths remain open phase-2 work. The overall
migration remains `PARTIAL`.

## 2026-08-15 Gap HTTP Status Classification Checkpoint

The sequential gap downloader now preserves a structured `RangeNotSatisfiable`
error for HTTP 416, keeps HTTP 5xx responses as `ServerError`, and maps other
HTTP failures to `HttpProtocolError` instead of a fatal configuration error.
This keeps result-code mapping and the shared retry policy consistent with the
ordinary sequential download path.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib engine::sequential_download::gap_download --all-features -- --test-threads=1
  3 passed, 0 failed
cargo test -p aria2-core --test test_e2e_gap_retry --all-features -- --test-threads=1
  26 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

Metalink status mapping and the remaining range/4xx paths still require a
separate audit. The overall migration remains `PARTIAL`.

## 2026-08-15 Metalink HTTP Status Classification Checkpoint

The two Rust-owned Metalink HTTP paths now map 5xx responses to structured
`ServerError` values and other HTTP failures to terminal `HttpProtocolError`
values instead of `FatalError::Config`. This aligns Metalink retry eligibility
and result-code mapping with ordinary sequential HTTP downloads.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib engine::metalink_download_command --all-features -- --test-threads=1
  19 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This is focused command-level evidence only. Full Metalink lifecycle,
cross-protocol, and original-client interoperability evidence remain open;
the overall migration remains `PARTIAL`.

## 2026-08-15 HTTP Redirect and Auth Error Classification Checkpoint

Sequential HTTP redirect failures without a `Location` header or with an
invalid target now return structured `HttpProtocolError` values. Auth-retry
redirect failures use the same protocol classification, and a post-auth 5xx
remains a retryable `ServerError` while other post-auth HTTP failures are
terminal protocol errors. Redirect following and credential resolution are
unchanged.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib engine::sequential_download::auth_retry --all-features -- --test-threads=1
  2 passed, 0 failed
cargo test -p aria2-core --lib engine::download_command --all-features -- --test-threads=1
  26 passed, 0 failed
cargo test -p aria2-core --test test_e2e_download test_e2e_redirect_without_location_is_http_protocol_error --all-features -- --exact --test-threads=1
  1 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

Full auth challenge coverage across schemes, proxy variants, and original
client interoperability remains open phase-2 work; the migration remains
`PARTIAL`.

## 2026-08-15 Autosave Control-file Flush Checkpoint

`SaveSessionCommand` now requests a control-file flush for every non-terminal
group before serializing the session. The active protocol command remains the
owner of its live checkpoint and consumes the request at its durable writer
boundary. The request/consume seam covers sequential and concurrent HTTP,
FTP/FTP proxy, SFTP, Metalink, and BitTorrent paths; it does not copy protocol
state into `RequestGroupMan`.

~~~text
cargo test -p aria2-core --lib auto_save --all-features -- --test-threads=1
  10 passed, 0 failed
cargo test -p aria2-core --lib progress_checkpoint --all-features -- --test-threads=1
  5 passed, 0 failed
cargo test -p aria2-core --lib filesystem::disk_writer --all-features -- --test-threads=1
  20 passed, 0 failed
cargo check -p aria2-core --all-targets --all-features
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

The autosave regression reads a real `.aria2` sidecar after the active
checkpoint owner consumes the request and verifies the updated completed
length. The product identity is now `aria2-rust 0.3.0` across the workspace,
CLI/RPC identity adapters, SDK metadata, installers, and distribution manifests.
This checkpoint remains phase-2 evidence only; live autosave timing, full
cross-protocol lifecycle E2E, original-client interoperability, and the
remaining matrix gaps keep the overall status `PARTIAL`.

## 2026-08-15 Sequential HTTP Cancellation Checkpoint

Sequential HTTP body reads now race every pending `bytes_stream` item against
the RequestGroup cancellation watcher. The in-memory metadata path uses the
same cancellation tick. Pause and remove can therefore stop a stalled
response without waiting for another body chunk. The existing
finalize-before-checkpoint ordering is shared by pending-read and
between-chunk cancellation paths.

Rust-owned verification:

~~~text
cargo test -p aria2-core --test test_e2e_download test_e2e_sequential_http_pause_interrupts_stalled_body_read --all-features -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --test test_e2e_download --all-features -- --test-threads=1
  28 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
cargo test --workspace --all-targets --no-run
  PASS
~~~

The Rust-owned slow-stream fixtures confirm that a paused sequential HTTP
download leaves a `.aria2` checkpoint with a strictly partial completed
length, while paused in-memory metadata does not create an output file. This
closes only the HTTP stalled-read slice; live cross-protocol lifecycle
coverage, original-client interoperability, and the remaining phase-2 gaps
keep the overall status `PARTIAL`.

## 2026-08-15 RequestGroup Autosave Terminal Filter Checkpoint

`RequestGroupMan::request_control_file_saves` now requests persistence only
for waiting, active, and paused groups. Complete, error, and removed groups
are excluded even before their scheduling entry is demoted, preventing stale
autosave requests from being attached to terminal tasks.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib request::request_group_man --all-features -- --test-threads=1
  32 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes only the terminal-state filtering slice of the phase-2
RequestGroup seam. Full cross-protocol lifecycle, session variants, and
original-client interoperability remain open, so the overall status remains
`PARTIAL`.

## 2026-08-15 Gap-download Cancellation Checkpoint

Concurrent HTTP fallback now reaches the Rust sequential gap downloader with
the completed ranges preserved. Both the ranged request and each pending body
chunk race the RequestGroup cancellation watcher; a pause or removal cleans
the partial gap and returns promptly instead of waiting for another network
chunk. The fixture forces a real `416` fallback and stalls the next ranged
body read.

Rust-owned verification:

~~~text
cargo test -p aria2-core --test test_e2e_gap_retry --all-features -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo test --workspace --all-targets --no-run
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes only the gap-download cancellation slice. Remaining protocol
command lifecycle coverage, session variants, integrity-dispatch callbacks,
cross-protocol E2E, and original-client interoperability keep phase 2 and the
overall migration `PARTIAL`.

## 2026-08-15 Integrity-dispatch Outcome Checkpoint

The Rust-owned integrity manager now has direct regression coverage for the
detailed `IntegrityOutcome` consumed by the BitTorrent command: a mixed file
with one verified and one mismatched piece reports `verified_piece_indices` and
`failed_piece_indices` without collapsing the result to a boolean. The
production repair path is covered for a failed single piece, a piece crossing
two physical files, and complete-payload hash-check seed controls.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib checksum::check_integrity -- --test-threads=1
  53 passed, 0 failed
cargo test -p aria2-core --features bittorrent --test test_e2e_bittorrent_download test_e2e_bt_check_integrity_redownloads_only_failed_piece -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --features bittorrent --test test_e2e_bittorrent_download test_e2e_bt_multi_file_integrity_repairs_piece_crossing_file_boundary -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --features bittorrent --test test_e2e_bittorrent_download test_e2e_bt_complete_integrity -- --test-threads=1
  2 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes the detailed integrity-outcome dispatch slice only. Remaining
integrity callback branches, protocol command lifecycle coverage, session
variants, cross-protocol E2E, and original-client interoperability keep phase
2 and the overall migration `PARTIAL`.

## 2026-08-15 Session follow-mode Round-trip Checkpoint

Session option serialization now has direct coverage for the three-valued
follow-mode contract at the session boundary. `follow-torrent=mem` and
`follow-metalink=false` are written using their canonical wire values and
restore to the corresponding typed `FollowMode` variants; the existing full
session file E2E remains green.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib session::session_entry --all-features -- --test-threads=1
  19 passed, 0 failed
cargo test -p aria2-core --test test_e2e_session --all-features -- --test-threads=1
  13 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes only the typed follow-mode session round-trip slice. Standard and
memory-backed Metalink graph variants, broader session lifecycle combinations,
cross-protocol E2E, and original-client interoperability remain open, so phase
2 and the overall migration stay `PARTIAL`.

## 2026-08-15 Cross-protocol E2E Checkpoint

The Rust-owned cross-protocol target passes the current local lifecycle
matrix: FTP anonymous/authenticated fallback, Metalink mirror failover and
SHA-256 verification, rate limiting, filename collision handling, disk-space
failure handling, and session save/restore all complete under their bounded
fixtures. The two ignored tests are the existing BitTorrent seeder fixture
cases and are not counted as passes.

Rust-owned verification:

~~~text
cargo test -p aria2-core --test deep_e2e_cross_protocol --all-features -- --test-threads=1
  18 passed, 0 failed, 2 ignored
~~~

This strengthens local cross-protocol evidence only. Full protocol lifecycle
combinations, third-party services, platform coverage, browser/original-client
interoperability, and the remaining phase-2 gates remain open; the overall
migration stays `PARTIAL`.

## 2026-08-15 CLI Short-Option Contract and Extension Boundary

The original short-option table is recorded in the checked-in Rust-owned
compatibility baseline. The reference implementation is used for the audit,
but tests do not read or build against `aria2_original`. Rust additionally keeps
explicit aliases for `listen-port`, RPC options, and BitTorrent seeding, peer
limits, and encryption. The exact Rust extension aliases are:

| Class | Short options | Long-option targets |
| --- | --- | --- |
| Original compatibility contract | `-a`, `-c`, `-d`, `-D`, `-i`, `-j`, `-k`, `-l`, `-m`, `-M`, `-n`, `-o`, `-O`, `-p`, `-P`, `-q`, `-R`, `-s`, `-S`, `-t`, `-T`, `-u`, `-U`, `-V`, `-x`, `-Z` | The mappings recorded in the Rust-owned compatibility baseline |
| Rust additive aliases | `-L`, `-e`, `-r`, `-I`, `-G`, `-g`, `-B`, `-X` | `listen-port`, `enable-rpc`, `rpc-listen-port`, `rpc-secret`, `seed-time`, `seed-ratio`, `bt-max-peers`, `bt-force-encryption` |

These aliases are additive product extensions and are tested separately, so
they cannot be mistaken for original compatibility claims. Original long
option names, configuration keys, defaults, and RPC behavior are unchanged;
this is a parser-boundary clarification, not a configuration change.

The option inventory is also split explicitly: the Rust-owned compatibility
baseline contains 214 public names; the all-features Rust registry contains
212 baseline names plus 22 Rust extensions, while `help` and `version` are
handled as CLI actions. The project-owned public-help inventory remains a
separate 198-name check and
does not include hidden/internal preference names.

The source metadata audit now matches the original deprecated set: `rpc-user`
and `rpc-passwd` are explicitly marked deprecated alongside the existing
deprecated option. Rust also retains two intentionally hidden product-owned
coefficient options, `optimize-concurrent-downloads-coeffA` and
`optimize-concurrent-downloads-coeffB`; they
are not claimed as original options.

Verification:

~~~text
cargo test -p aria2 --test test_cli_options --all-features -- --test-threads=1
  109 passed, 0 failed
cargo test -p aria2-core --all-features --lib config::option -- --test-threads=1
  45 passed, 0 failed
cargo fmt --all -- --check
  PASS
~~~

The CLI/options matrix remains `PARTIAL`: exact defaults, help-tag text,
feature-conditional runtime behavior, and original-client end-to-end
interoperability still require evidence. The four runtime-policy sets are now
verified item-by-item against the Rust-owned compatibility baseline (`113/113`,
`120/120`, `106/106`, and `7/7`), with the documented Rust extensions checked
separately.
The short-option boundary and current compatibility inventory are verified from
Rust-owned fixtures; they do
not claim full option semantic parity.

## 2026-08-14 HTTP Redirect Contract Checkpoint

The Rust protocol response seam now matches
`aria2_original/src/HttpResponse.cc`: `HttpResponse::is_redirect()` requires
both a recognized redirect status (`300/301/302/303/307/308`) and a `Location`
header. `304 Not Modified` remains outside the redirect set. The standalone
Rust redirect helper now includes `300 Multiple Choices`, and core
skip-response classification uses the shared status predicate so a recognized
redirect without `Location` still produces the original protocol error path
instead of being silently consumed.

This is an internal Rust implementation correction at the external HTTP
behavior seam. It does not change option names, defaults, user configuration,
RPC/CLI wire behavior, or the `aria2-rust 0.3.0` product identity, and it does
not copy the C++ command state machine.

Verification on 2026-08-14:

~~~text
cargo test -p aria2-protocol --tests --all-features -- --test-threads=1
  829 library + 6 integration + 53 uTP tests passed, 0 failed
cargo test -p aria2-core --lib http::skip_response --all-features -- --test-threads=1
  35 passed, 0 failed
cargo test -p aria2-core --test deep_e2e_http --all-features -- --test-threads=1
  12 passed, 0 failed
cargo clippy -p aria2-protocol --all-targets --all-features -- -D warnings
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

The broader HTTP and original-client/browser interoperability matrix remains
`PARTIAL`.

## 2026-08-14 HTTP Transfer-Encoding Contract Checkpoint

The Rust HTTP body-filter seam now validates `Transfer-Encoding` before any
response body is consumed. It accepts only the original aria2-supported
case-insensitive single value `chunked`; `gzip`, `deflate`, `bzip2`, `br`,
`identity`, unknown values, and multi-token values are rejected with an HTTP
protocol error. Transfer decoding runs before the independent
`Content-Encoding` filters, matching the original response pipeline while
keeping the implementation Rust-native.

The empty-body path is validated as well. No option name, default, user
configuration, RPC/CLI wire behavior, or `aria2-rust 0.3.0` product identity
was changed.

Verification on 2026-08-14:

~~~text
cargo test -p aria2-core --lib http::stream_filter_tests --all-features -- --test-threads=1
  31 passed, 0 failed
cargo test -p aria2-core --lib http::skip_response --all-features -- --test-threads=1
  36 passed, 0 failed
~~~

The broader HTTP body-stream integration and original-client/browser
interoperability matrix remains `PARTIAL`.

## 2026-08-14 RPC Lifecycle Commit Checkpoint

The Rust RPC lifecycle seam now commits the externally visible transition
before returning, matching the synchronous observation point of
`aria2_original/src/RpcMethodImpl.cc`. `aria2.forcePause` reports a task as
`paused` immediately; `aria2.remove` and `aria2.forceRemove` validate the GID
and remove reserved tasks into the stopped-result store immediately, while
active tasks remain indexed until the engine drains their protocol command.
Unknown `forceRemove` GIDs now return aria2 execution error code `1`. Single
task `pause` and `forcePause` now reject already-paused or terminal tasks, and
`unpause` rejects waiting, active, and terminal tasks; these transitions return
the original execution error code `1` instead of silently succeeding.

The engine command channel remains responsible for protocol-specific
cancellation, checkpoint finalization, and completion accounting. This is a
Rust-native ordering correction behind the RPC seam; it changes no option
name, default, user configuration, product version, or original wire shape.

Verification on 2026-08-14:

~~~text
cargo test -p aria2-rpc --test integration_rpc --all-features -- --test-threads=1
  19 passed, 0 failed
cargo test -p aria2-rpc --lib handlers::handler_tests --all-features -- --test-threads=1
  71 passed, 0 failed
cargo test -p aria2-rpc --test test_e2e_all_rpc_methods --all-features -- --test-threads=1
  55 passed, 0 failed
cargo test -p aria2-core --lib request::request_group_man --all-features -- --test-threads=1
  31 passed, 0 failed
cargo test -p aria2-core --lib engine::engine_loop --all-features -- --test-threads=1
  14 passed, 0 failed
cargo clippy -p aria2-core -p aria2-rpc --all-targets --all-features -- -D warnings
  PASS
~~~

The broader original-client, browser-extension, XML-RPC, and WebSocket
interoperability matrix remains `PARTIAL`.

## 2026-08-14 Client TLS Transport Checkpoint

The existing aria2-compatible `check-certificate`, `ca-certificate`,
`certificate`, and `private-key` options now flow through one Rust-owned TLS
configuration helper for primary HTTP/HTTPS downloads, all Metalink HTTP
client construction paths, production BitTorrent HTTP tracker announces, and
BT web-seed clients.
The helper strictly parses CA PEM bundles, installs every root, applies
verification-disabled mode, validates the client certificate/private-key pair,
and reports configuration errors without changing option names, defaults,
session format, or user configuration behavior. This is a transport
implementation detail of the Rust engine, not a copied C++ module or a new
configuration surface. When `private-key` is omitted, the helper now parses
the original empty-password PKCS#12 single-file identity form with a pure-Rust
adapter, preserves its certificate chain, and presents it through Rustls. The
verified matrix includes legacy SHA-1/3DES PFX, PBES2 with PBKDF2-HMAC-SHA1 or
PBKDF2-HMAC-SHA256 and AES-256-CBC, and both empty-password BMP encodings. A
checked-in Rust-native Rustls fixture also drives a live local HTTPS server
and verifies custom CA trust, disabled server-certificate verification,
separate PEM mutual TLS, and legacy single-file PKCS#12 mutual TLS through the
same helper. AES-128/192-CBC, AES-GCM, alternative PBKDF2 PRFs, plaintext
keyBag, unsupported bag types, and the broader external client matrix remain
explicitly open.

~~~text
cargo test -p aria2-core --lib http::client_identity --all-features -- --test-threads=1
  15 passed, 0 failed
cargo test -p aria2-core --lib engine::http_tracker_client --all-features -- --test-threads=1
  13 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_tracker_comm::tracker_announce --features bittorrent -- --test-threads=1
  4 passed, 0 failed
cargo test -p aria2-core --lib engine::download_command --all-features -- --test-threads=1
  23 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_web_seed --all-features -- --test-threads=1
  14 passed, 0 failed
cargo test -p aria2-core --lib --all-features -j 1 -- --test-threads=1
  3358 passed, 0 failed, 1 ignored
cargo check -p aria2-core --all-features --tests -j 1
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

The PEM and empty-password PKCS#12 construction and validation seams plus
local live HTTPS fixtures are covered. The modern fixture verifies PBES2 with
AES-256-CBC and SHA-256-based PBKDF2; it is a construction test rather than an
external-client interoperability claim. AES-128/192-CBC, AES-GCM, alternative
PBKDF2 PRFs, plaintext keyBag, unsupported bag types, and the broader
original-client HTTPS matrix remain open.

## 2026-08-14 Retry Policy Internal Seam Checkpoint

The Rust-owned retry policy now uses one millisecond-preserving backoff
implementation for both `compute_wait` and `wait_duration`. Custom backoff
factors and sub-second policies are covered without changing the public
`max-tries` contract: the value still counts total attempts and `0` remains
unlimited. This is internal Rust cleanup; option names, defaults, and wire
behavior are unchanged.

~~~text
cargo test -p aria2-core --lib engine::retry_policy --all-features -- --test-threads=1
  16 passed, 0 failed
cargo test -p aria2-core --test test_retry --all-features -- --test-threads=1
  15 passed, 0 failed
cargo test -p aria2-core --test test_error_network --all-features -- --test-threads=1
  32 passed, 0 failed, 2 ignored
~~~

## 2026-08-14 Mirror Statistics Protocol-Key Checkpoint

The Rust concurrent-mirror path now uses the same structured URL parsing seam
for ServerStat feedback and lookup. Successful segment speed, failure state,
and connection-rebalancing reads are keyed by `(hostname, protocol)`, so HTTP
and HTTPS mirrors with the same hostname do not share a statistic accidentally.
Structured `ServerError` codes are retained in failure feedback, with explicit
coverage for `416` and timeout `408`; failures without a status keep the
existing `500` fallback without parsing human-readable error text.
This is an internal Rust scheduling correction. It changes no option name,
default, configuration-file behavior, RPC field, or product identity.

~~~text
cargo test -p aria2-core concurrent_segment_manager --lib --offline
  23 passed, 0 failed
cargo test -p aria2-core mirror_coordinator --lib --offline
  11 passed, 0 failed
cargo fmt --all -- --check
  PASS
~~~

The broader migration remains `PARTIAL`: DNS candidate failure attribution,
bad-address eviction/refresh across every protocol, redirect/auth precedence,
and original-client interoperability still require separate evidence.

## 2026-08-14 Sequential HTTP Conditional-GET Checkpoint

The Rust sequential HTTP path now uses the same exact redirect status set as
the response-validation seam: `300`, `301`, `302`, `303`, `307`, and `308`.
`304 Not Modified` is intentionally handled as a conditional-cache result,
so it no longer requires a `Location` header or enters redirect accounting.
This preserves the existing local file and completes the request after a
valid conditional response. The change is internal Rust routing; it does not
change option names, defaults, user configuration, product identity, or RPC
wire behavior.

~~~text
cargo test -p aria2-core --lib http::response --all-features --offline -- --test-threads=1
  88 passed, 0 failed
cargo test -p aria2-core --lib engine::download_command::tests::conditional_get_304_completes_without_location --all-features --offline -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --lib engine::download_command::tests::unconditional_304_is_rejected_as_http_protocol_error --all-features --offline -- --exact --test-threads=1
  1 passed, 0 failed
cargo fmt --all -- --check
  PASS
~~~

The HTTP response/validation focused tests cover this status-code seam. The
complete redirect/auth precedence matrix, other HTTP status edge cases,
and original-client interoperability remain open under the broader `PARTIAL`
HTTP status.

## 2026-08-14 Sequential HTTP Authentication-Redirect Checkpoint

Authentication retries in the Rust sequential download path now return a
bounded redirect action to the existing manual redirect loop. A `401/407 ->
Authorization -> 3xx` response therefore follows the same redirect limit,
URI tracking, cookie handling, and target request construction as an initial
redirect; it is not handled by recursive retry logic. The task-owned auth
factory remains alive across the transition, and Basic credential matching uses
the original directory-based protection-space rules. Cross-host credential
isolation remains explicit.
This is an internal Rust control-flow improvement and does not change option
names, defaults, user configuration, product identity, or RPC wire behavior.

~~~text
cargo test -p aria2-core engine::download_command --lib --offline
  26 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features --offline -- -D warnings
  PASS
~~~

The covered `401 -> 302 -> 200` fixture proves same-directory protection
space reuse. Cross-host credential isolation, the complete 401/407 scheme and
redirect precedence matrix, and original-client interoperability remain
`PARTIAL`.

## 2026-08-14 DNS Candidate Refresh Checkpoint

The Rust `DnsCache` now exposes one `resolve_with_refresh` seam. It preserves
the original candidate state while usable addresses remain, but removes the
endpoint cache and resolves again when all cached candidates have been marked
bad. HTTP task creation and FTP control retries use this same seam. This is an
internal Rust ownership change matching the original connection lifecycle; it
does not change option names, defaults, user configuration, product identity,
or RPC wire behavior.

~~~text
cargo test -p aria2-core --lib dns::dns_cache --all-features --offline -- --test-threads=1
  21 passed, 0 failed
cargo test -p aria2-core --lib engine::ftp_download_command --all-features --offline -- --test-threads=1
  18 passed, 0 failed
cargo fmt --all -- --check
  PASS
~~~

The HTTP selected-peer attribution for connection failures, complete DNS
candidate failure coverage across every protocol, and original-client
interoperability remain `PARTIAL`.

## 2026-08-14 Selected-Peer Timeout Attribution Checkpoint

Request groups still retain the observed connection history for diagnostics,
but timeout housekeeping now marks only the latest active peer instead of
marking every peer observed by a concurrent or mirror-aware command
generation. Re-observing a peer moves it to the active end of that history.
This removes the known false eviction of healthy DNS candidates without
changing options, defaults, user configuration, product identity, or RPC wire
behavior.

~~~text
cargo test -p aria2-core request::request_group --lib --offline
  95 passed, 0 failed
cargo test -p aria2-core engine::engine_loop --lib --offline
  14 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features --offline -- -D warnings
  PASS
~~~

The reqwest DNS-pinned connector still does not expose the selected address
when connection establishment fails before a response, so exact connection
failure attribution and full protocol coverage remain `PARTIAL`.

## 2026-08-14 FTP Rust-native cleanup checkpoint

The unused FTP capability stubs that still claimed `AUTH TLS` and `PROT P`
were not implemented have been removed from the negotiation helper. Production
FTPS remains owned by the Rust `connection/tls.rs` path, which performs the
actual control and data-channel upgrades; no option, default, or wire behavior
changed.

~~~text
cargo test -p aria2-core --all-features --lib ftp::connection::negotiation
  33 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

## 2026-08-14 FTP active-mode interface checkpoint

The fresh and pooled FTP active-mode paths now bind their data listeners to
the local IP address selected by the control connection. This matches
`aria2_original/src/FtpConnection.cc::createServerSocket` and prevents a
wildcard listener from advertising the wrong interface on multi-homed hosts.
The binding policy is a small Rust-owned helper shared by both async paths;
it does not copy the original state machine and changes no option, default,
configuration, RPC wire behavior, product identity, or session format.

Verification:

~~~text
cargo test -p aria2-core --lib ftp::connection::negotiation --all-features --offline -- --test-threads=1
  34 passed, 0 failed
cargo fmt --all -- --check
  PASS
~~~

This closes only the local listener-binding regression. The later production
active-mode E2E checkpoint below covers the real engine path; third-party
FTP/FTPS servers, multi-homed process coverage, and original-client
interoperability remain unverified, so FTP/FTPS remains `PARTIAL` in the matrix.

## 2026-08-14 FTP production PWD/CWD checkpoint

The production Rust FTP command now follows the original public FTP command
order after `TYPE I`: it queries `PWD`, traverses the base working directory
and URI directory components with `CWD`, then sends `SIZE` and `RETR` with the
decoded file name. Data-channel preparation now precedes `REST`; passive data
TCP is established before `REST`, active mode advertises its listener before
`REST`, and `REST 0` is sent for fresh downloads as in the original. The path
split, CWD target construction, and PWD response parsing are shared Rust-owned
helpers; the production adapter does not copy the original C++ state machine.
No option, default, user configuration, RPC wire behavior, product version, or
session format changed.

The fixture can reject absolute paths and requires this sequence. The real
production engine passes that E2E, including the existing passive/active,
resume, checksum, lifecycle, and error regressions.

~~~text
cargo test -p aria2-core --lib engine::ftp_download_command --all-features --offline -- --test-threads=1
  20 passed, 0 failed
cargo test -p aria2-core --lib ftp::connection::negotiation --all-features --offline -- --test-threads=1
  39 passed, 0 failed
cargo test -p aria2-core --test test_e2e_ftp_download --all-features --offline -- --test-threads=1
  32 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test ftp_integration_test --all-features --offline -- --test-threads=1
  13 passed, 0 failed
cargo clippy -p aria2-core --lib --all-features --offline -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

## 2026-08-14 FTP `remote-time` checkpoint

The production Rust FTP path now queries `MDTM` after the original `PWD`/`CWD`
traversal and before `SIZE` when the existing `remote-time` option is enabled.
A valid RFC 3659 timestamp is applied to the completed local file after the
Rust writer releases its handle; unsupported or malformed optional responses
continue without changing download success. The timestamp parser is shared
with the existing FTP negotiation seam. This preserves the existing option,
default, configuration, RPC wire behavior, product identity, and Rust-owned
internal architecture.

~~~text
cargo test -p aria2-core --test test_e2e_ftp_download test_e2e_ftp_remote_time_applies_mdtm_timestamp --all-features -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  35 passed, 0 failed, 2 ignored
~~~

This closes only the production FTP `remote-time`, `dry-run`, and
`connect-timeout` behavior. Third-party FTP/FTPS interoperability, multi-homed
process coverage, and original-client interoperability remain open, so FTP/FTPS
remains `PARTIAL`.

## 2026-08-14 Metalink Session Graph Persistence Checkpoint

The application-level legacy session path now has a real save/restart/restore
regression for a memory-backed Metalink torrent graph.
`save_session_on_shutdown` persists the metadata URI/GID identity and
Rust-owned graph descriptors, while `restore_session` rebuilds the metadata
prerequisite before the dependency-gated payload. The test also verifies that
only the payload entry is written and that the restored payload remains
unresolved until metadata completion. No option name, default, or user
configuration format was changed.

Verification on 2026-08-14:

~~~text
cargo test -p aria2 --all-features --lib app::tests::test_session_save_then_restart_restores_metalink_graph -- --exact --test-threads=1
  1 passed, 0 failed
~~~

This closes the standard memory-backed graph save/restore slice. Full
`follow-torrent=mem` semantics, other Metalink lifecycle variants, and live
original-client interoperability remain open.

The transparent BitTorrent source path also has a real HTTP `follow-torrent=mem`
regression: the source metadata stays in memory, is parsed by the Rust-owned
post-download handler, and creates a child payload without a source file on
disk. This is a covered path, not a claim that every protocol, failure mode,
or original-client combination is complete.

Verification on 2026-08-14:

~~~text
cargo test -p aria2-core --all-features --test deep_e2e_bittorrent follow_torrent_mem_http_creates_child_without_source_file -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_metalink_lifecycle -- --test-threads=1
  13 passed, 0 failed, 2 ignored
~~~

## 2026-08-14 Configuration Parsing and Validation Checkpoint

The configuration registry remains Rust-owned. This checkpoint did not change
option names, product defaults, user configuration files, or the
`aria2-rust 0.3.0` release identity.

The explicit-value seam now has a separate default-value path. CLI, config-file,
and environment input all pass explicit text through `OptionDef::parse_value`;
`ConfigParser::apply_defaults` is the only default injection stage and uses
`parse_default_value`. Empty text is therefore not silently converted into an
option default. Boolean input accepts the original exact `true`/`false` values;
an empty value is treated as enabled only for a boolean flag. `rpc-secret` is
explicitly non-empty. Boolean flags also no longer consume the next
space-separated positional argument.

Focused evidence already recorded in this checkout includes 42 option tests,
30 parser tests, 48 config-file regressions, 105 CLI regressions, 232 RPC
library tests, 18 RPC integration tests, 55 all-method RPC E2E tests, and 47
HTTP/WebSocket/XML-RPC route E2E tests. The
RPC concurrency target was rerun directly on 2026-08-14:

~~~text
cargo test -p aria2-rpc --test test_stress_rpc_concurrent -- --nocapture
  10 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo clippy -p aria2-protocol --all-targets --all-features -- -D warnings
  PASS
cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings
  PASS
cargo clippy -p aria2 --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

The same validation round also passed the current RPC and CLI regression
targets; the latest `aria2-rpc` library target is `232 passed, 0 failed`, the full `aria2`
all-features test target, and SFTP E2E `12 passed, 0 failed`. These are
current checkout results for this cleanup round; they do not close the
remaining original-client interoperability or workspace acceptance gates.

This is a completed configuration-validation slice, not overall migration
completion. Original-client interoperability, full protocol lifecycle
coverage, workspace E2E, and performance comparison remain open.

## 2026-08-14 Obsolete Option Handler Cleanup

The former `aria2-core::option::OptionHandler` Rust module was removed after a
workspace-wide reference audit. It had no production callers, examples,
bindings, or external adapter references; its only remaining users were its
own eight tests. The active CLI, config-file, environment, RPC, session, and
download-option paths already use the canonical `config::OptionRegistry`,
`ConfigParser`, and typed `OptionDef` seam. Removing the duplicate default table
and auto-detect parser reduces drift risk and does not change any user option,
default, wire shape, or `aria2-rust 0.3.0` identity.

Deletion verification on 2026-08-14:

~~~text
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3355 passed, 0 failed, 1 ignored
cargo test -p aria2-rpc --all-features --lib -- --test-threads=1
  228 passed, 0 failed
cargo test -p aria2 --all-features --tests -- --test-threads=1
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings
  PASS
~~~

Historical references to C++ `OptionHandler` remain source-comparison
terminology only; no Rust compatibility shim was retained.

## 2026-08-14 XML-RPC Value-State Compatibility Checkpoint

The XML-RPC parser now applies a Rust-owned `decode_aria2_base64` helper to
`<base64>` values. It matches the original `base64.h` behavior relevant to
clients: non-alphabet bytes are ignored, incomplete trailing input follows the
original permissive result, and padding is validated before decoding. The
helper is shared with the existing JSONP GET adapter; the C++ parser state
machine was not copied into Rust.

The same Rust-owned parser now preserves the original omission boundary for
well-formed requests whose current value is unusable: invalid or out-of-range
integers, unknown value nodes, and struct members with a missing or empty name
or missing value are omitted from the current frame. A complete XML document
therefore reaches normal method execution and returns the original XML fault
contract; only malformed XML or conversion failure of the document itself
returns HTTP 400 with an empty body. `XmlRpcResponse::array_val` also emits one
array-valued `<param>`, matching the original response shape.

Source-backed evidence from the current tree:

~~~text
cargo test -p aria2-rpc --all-features --lib xml_rpc -- --test-threads=1
  19 passed, 0 failed
cargo test -p aria2-rpc --test test_e2e_http_server --all-features -- --test-threads=1
  47 passed, 0 failed
cargo test -p aria2-rpc --test test_e2e_all_rpc_methods --all-features -- --test-threads=1
  55 passed, 0 failed
cargo test -p aria2-rpc --test test_e2e_rpc_server --all-features -- --test-threads=1
  5 passed, 0 failed
~~~

This closes the Base64 and value-state differences covered here only. The RPC
area remains `PARTIAL` until the complete original-client, browser-extension,
XML-RPC fault/parameter, and protocol lifecycle matrices are reproducibly
green.

## 2026-08-14 CookieStorage Concurrency and Lookup Checkpoint

The canonical Rust `CookieStorage` remains the production cookie model. Its
domain keys are normalized at the storage boundary, and request lookup now
walks only the host's possible domain suffixes instead of scanning every
stored domain. This preserves aria2's host-only/domain-cookie checks, secure
filtering, and RFC path/creation ordering while reducing lookup work for a
process-wide cookie store. Domain eviction now follows the same
`domains -> lru` lock order as insertion, expiry, and clearing, removing a
concurrent deadlock hazard at the global eviction threshold. The older
`CookieJar`/`JarCookie` types remain only as an API/session adapter; download
execution uses the canonical storage model.

Verification on 2026-08-14:

~~~text
cargo test -p aria2-core cookie::tests_storage --lib --all-features -- --test-threads=1
  35 passed, 0 failed
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the covered cookie-storage concurrency and lookup slice. HTTP
remains `PARTIAL` until broader original-client, browser-extension, and full
redirect/protocol interoperability evidence is complete.

## 2026-08-14 Rust-Native Session Transfer Statistics Checkpoint

The source comparison found a real difference in the original
`DownloadContext::updateDownload` / `updateUploadLength` /
`updateUploadSpeed` path: C++ forwards those updates through a raw owner
pointer into `RequestGroupMan::NetStat`, while Rust had only local counters and
TODO comments. Rust now keeps the ownership model independent: each
`RequestGroupMan` owns one lock-free `GlobalNetStat`, registered groups receive
an `Arc` to it, and `DownloadContext` can update it without a raw back-pointer.
The HTTP progress updater records only new absolute bytes and initializes its
baseline from the restored offset, so resume state is not counted twice.

This is an internal Rust architecture change. It does not copy the C++ class
hierarchy, change option definitions/defaults, alter config-file behavior, or
change JSON-RPC/XML-RPC/WebSocket field names or `aria2.getGlobalStat`'s wire
shape. Current live global speed reporting remains based on each group's
lock-free speed cache; the new session counters are an engine-owned seam for
protocol and CLI statistics.

Verification on 2026-08-14:

~~~text
cargo test -p aria2-core --lib download_context
  42 passed, 0 failed
cargo test -p aria2-core --lib request_group
  97 passed, 0 failed
cargo clippy -p aria2-core --lib --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

The full core library run reached 2474 passed tests but still has two existing
configuration-test failures (`test_registry_identity_defaults_match_original_aria2`
and `runtime_policy_names_are_registered_without_duplicates`). Those failures
are intentionally not bypassed by changing configuration definitions in this
checkpoint.

## Current Module Matrix

| Area | Rust implementation | Status | Main evidence or remaining gap |
| --- | --- | --- | --- |
| Engine and scheduling | aria2-core/src/engine/ | PARTIAL | Typed command loop, generation-based completion accounting, `CancellationToken` shutdown, pause/unpause requeueing, runtime concurrency and global rate updates are covered. Shared retry policy now has source-backed `max-tries` semantics across sequential HTTP, concurrent segments, and FTP; full parity across allocation and all protocol commands is not yet proven. |
| HTTP/HTTPS | aria2-core/src/http/, aria2-protocol/src/http/ | PARTIAL | Focused parser and download coverage exists, including existing-file naming, control-file cleanup, preallocation-safe resume recovery, unknown-remote-length resume, multi-URI resume failover, HTTP 200 responses that ignore a requested Range (CannotResume by default or fresh restart according to always-resume/max-resume-failure-tries), request-level GET/HEAD, cache, digest, keep-alive, explicit-header, gzip, chunked, and canonical CookieStorage coverage. Existing `check-certificate`, `ca-certificate`, `certificate`, and `private-key` options now use one Rust-owned TLS config in primary HTTP/HTTPS and Metalink clients, plus production BitTorrent HTTP tracker and web-seed clients; verification-disabled mode, strict multi-root PEM CA loading, separate PEM and legacy empty-password PKCS#12 client identities, PBES2/AES-256-CBC PFX construction, configuration errors, and local live HTTPS fixtures for custom CA, disabled verification, and legacy identity forms are covered. AES-128/192-CBC, AES-GCM, alternative PBKDF2 PRFs, plaintext keyBag, unsupported bag types, and the broader original-client HTTPS matrix remain unverified. Cookie lookup is host-suffix indexed with normalized domain keys, and domain eviction uses one lock order; the legacy CookieJar is only an API/session adapter. An E2E check proves max-tries counts total GET attempts with 0 meaning unlimited. Default production clients explicitly disable gzip negotiation and opt in only through http-accept-gzip; every unknown-length path, including explicit split > 1, starts with one ordinary GET and remains on the original single-connection unknown-length path without a synthetic Range probe. Concurrent buffered and streaming Range requests now share a bounded manual redirect seam, preserve Range validation after redirects, and propagate redirect Set-Cookie values through the task cookie store; 401/407 responses use the original authentication result mapping and the existing challenge credential retry seam for segmented requests. HTTP, HTTPS, and ALL proxy selection, proxy credentials from explicit options or proxy-URL userinfo, no-proxy matching, manual redirects, and real authenticated-proxy E2E coverage are implemented; a production E2E also proves proxy-URL credentials remain available for a 407 fallback. Rust's internal GrowSegment/unknown-length storage modules are not yet the production writer seam; this is an internal architecture difference, not a missing download path. Core owns production orchestration; aria2-protocol::http::client is the standalone adapter used by legacy protocol helpers, and broader original-binary interoperability remains unverified. |
| FTP/FTPS | aria2-core/src/ftp/, aria2-protocol/src/ftp/ | PARTIAL | Original FTP active/passive/auth behavior has focused coverage, including the canonical `PWD`/directory-level `CWD`/file-name `SIZE` and `RETR` order, optional `remote-time` `MDTM` query and local mtime application, FTP `dry-run` metadata-only completion without `REST`/`RETR`, `connect-timeout` enforcement for silent control peers, multiline response parsing with the C++ 64 KiB receive limit, the original PASV control-peer target rule, active-mode listeners bound to the control connection's local interface, `max-tries` total-attempt semantics, remote `SIZE` versus `RETR` length validation, whole-file checksum verification for both fresh downloads and same-length local-file short-circuiting, and real slow-server pause/remove/unpause lifecycle E2E (`test_e2e_ftp_download`: 36 passed, 2 ignored). The Rust command now persists partial progress through the internal `A2CF` checkpoint seam and removes the checkpoint only after successful completion. Live third-party-server, multi-homed process, and original-client interoperability evidence is incomplete. FTPS is a Rust-only additive extension: explicit/implicit control and data TLS paths exist, the plaintext downgrade regression is covered, and positive TLS-server interoperability is still unverified. |
| SFTP | aria2-protocol/src/sftp/, aria2-core/src/engine/sftp_download_command/ | PARTIAL | A local `russh` SFTP server E2E verifies password acceptance and rejection, aria2_original's `sha-1=<hex>` host-key pin acceptance and mismatch rejection, missing-file mapping, complete output, resume from an existing local prefix, configured whole-file checksum verification after transfer, and real slow-server pause/remove/unpause lifecycle (`test_e2e_sftp_download`: 12 passed, 0 failed). A complete local output with a matching checksum is accepted before any SFTP `READ`; a mismatch resets the resume offset and returns to the remote transfer path. The Rust command persists partial progress through the internal `A2CF` checkpoint seam and removes it only after successful checksum-verified completion. Third-party SFTP server interoperability and the complete original error matrix remain unverified. Rust's protocol crate has an additive public-key authentication API, but aria2_original exposes no SSH private-key login option; `--private-key` remains an HTTP/HTTPS client-TLS option. Known-hosts persistence is not part of aria2_original's `ssh-host-key-md` contract. |
| BitTorrent | aria2-protocol/src/bittorrent/, aria2-core/src/engine/bt_* | PARTIAL | Core protocol pieces exist. `index-out` now applies the original 1-based `INDEX=PATH` mapping to both `DownloadContext` and the actual single/multi-file writers; TCP listen-port ranges try ports in order and have occupied-port regression coverage. `bt-prioritize-piece` now uses the original typed `head[=SIZE],tail[=SIZE]` parser and a file-boundary priority wrapper over rarest-first, with focused parser/picker/index tests. The process listener now owns one shared TCP socket, routes MSE and legacy handshakes by info-hash, unregisters routes with RAII, and releases its port on shutdown. MSE covers PadA/PadB, RC4 and plaintext-after-MSE negotiation, `bt-force-encryption`, `bt-require-crypto`, and `bt-min-crypto-level`; focused socket and state-machine evidence is recorded below. Rust A2CF checkpoints now bind the info-hash, reject malformed trailing bits, require payload presence, restore piece-sized progress, persist peer and web-seed completions, and are exercised through halt, pause/resume, verified-piece skip, no-peer web-seed download, failed-piece integrity recovery, complete-payload hash-check controls, and a real multi-file piece crossing two physical files. Explicit `SaveSessionCommand` requests now flush the positioned/cache-backed writer before publishing the verified-piece sidecar; the Rust-owned save-session web-seed regression proves the checkpointed piece is readable after that boundary. A successful complete integrity check emits the BT completion hook only when `bt-enable-hook-after-hash-check=true`; `bt-hash-check-seed=false` completes locally without tracker/peer discovery, while the default `true` path enters a real tracker/peer lifecycle. The command-level suite now reports `30 passed, 0 failed, 2 ignored`; the obsolete context-free PieceStorage filter setup seam was removed because production BT selection is Rust-owned by `DownloadContext -> allowed_piece_indices -> PiecePicker`. Dependency graph, full scheduler/seeding parity, and live original-client interoperability remain open. |
| DHT and trackers | aria2-protocol/src/bittorrent/dht/, aria2-protocol/src/bittorrent/tracker/ | PARTIAL | Production paths and tests use the protocol crate as the single canonical DHT implementation. The former unreferenced `aria2-core/src/dht/` duplicate was removed after a source/dependency audit; no public wire, configuration, default, or product-version behavior changed. DHT port ranges now try the ordered list and fall back after an occupied first port. The Rust-only public tracker catalog is wired through the BT announce path with source refresh, URL de-duplication, HTTP/UDP dispatch, private-torrent exclusion, disabled/enabled availability, exponential health backoff, and success recovery; these `enable-public-trackers`/`bt-tracker-source` options are additive extensions and do not alter original-client requests. Complete live-network and original-client interoperability evidence is still missing. |
| Metalink | aria2-protocol/src/metalink/, aria2-core/src/engine/metalink_* | PARTIAL | V3/V4 parsing, filtering, resource downloads, manager-owned GID allocation, relative-URI base propagation, and metadata/payload graph terminal states have focused regression coverage. Ordinary HTTP payloads now stream through the Rust disk-writer seam, persist pause/remove progress in Rust `A2CF`, resume with `Range`, remove the checkpoint on success, and verify whole-file and `<pieces>` hashes by streaming the output file. Named shared metaurls now form one multi-file payload with per-file direct-mirror and original-name mappings, and the original `metalink4-groupbymetaurl.xml` shape is covered. Both manager-owned `BtDependency` resolution and command-level direct-mirror fallback reuse one torrent-context mapping seam; a local HTTP regression proves that a failed shared group requests one torrent metadata resource and preserves every file path/name/URI mapping. A process-level E2E now submits `EngineCommand::AddMetalinkGraph`, verifies one metadata request, promotion-time context injection, the mapped output path, and a web-seed payload completion (`13 passed, 0 failed, 2 ignored`). The application session path now proves save/restart/restore of a standard memory-backed graph (`test_session_save_then_restart_restores_metalink_graph`: 1 passed), including metadata-first dependency reconstruction. Zero-length torrent payloads complete without peer discovery. Full `follow-torrent=mem` semantics, other Metalink lifecycle variants, and live protocol interoperability remain open. |
| Integrity and resume | aria2-core/src/checksum/, aria2-core/src/session/ | PARTIAL | Sequential resume detection, defunct-control-file cleanup, existing-file policy, preallocation-safe offset writes, `always-resume`, and `max-resume-failure-tries` multi-URI behavior have focused unit/HTTP E2E evidence. Single- and multi-mirror concurrent HTTP paths, ordinary Metalink payloads, and SFTP now create/load, checkpoint, flush on cancellation or Range fallback, restore compatible prefixes or segment bitfields, discard untrusted sidecars, and remove `.aria2` only after successful completion and checksum verification where configured; two real multi-mirror HTTP cases, the Metalink lifecycle E2E, and SFTP checksum preflight/transfer cases verify restored data is not incorrectly accepted. Metalink whole-file and piece hashes are checked through streaming file reads. Session serialization preserves original option names and non-default values for resume policy, trackers, port ranges, piece sizing, FTP/auth/netrc settings, plus the original 16-hex-digit GID form; Rust-only fields remain extensions. The result-code seam now contains exactly the original wire values `0..32`; `paused` remains a separate task status and cannot leak a Rust-only error code. Live engine pause/remove orchestration across every protocol, checksum-integrity dispatcher callbacks beyond the covered paths, and broader original-client interoperability remain incomplete or unverified. |
| RPC and WebSocket | aria2-rpc/src/ | PARTIAL | JSON-RPC/XML-RPC/WebSocket surfaces, token/Basic authentication, aria2-compatible error/status mapping, feature-specific method/notification discovery, feature-aware `getVersion`, browser-facing CORS preflight headers, and real HTTP E2E coverage exist. CORS is disabled by default as in `aria2_original`; explicit `rpc-allow-origin-all=true` enables wildcard headers, and `Access-Control-Max-Age` matches the original at `1728000`. XML-RPC execution faults use HTTP 200 + `faultCode=1`; structurally malformed documents or conversion failures use the original HTTP 400 empty-body contract, while well-formed documents with invalid scalar/member values follow the original omission semantics before normal method execution. `getServers` is active-only and reports only real in-flight requests; waiting, paused, stopped, or unknown GIDs return execution error code 1. `getSessionInfo` now generates one 20-byte random session key per engine and exposes the original 40-character lowercase hexadecimal representation. The catalog is 33 core methods plus 2 BitTorrent and 1 Metalink method when enabled; notifications are 5 core plus 1 BitTorrent event when enabled. `aria2.forceUnpause` is rejected as an unknown original method and omitted from `system.listMethods`, keeping original-client discovery exact. Task creation and runtime changes share core validation; `RequestGroup` owns a source-derived `setInitialOption(true)` request snapshot and transfers its effective state to `DownloadResult` when a task stops, excluding process-only RPC settings and Rust-only session metadata. `getOption` therefore keeps the original task state for both live and stopped GIDs, including only changes already applied to the task; later `changeGlobalOption` calls affect future tasks without rewriting existing ones. `getGlobalOption` uses registry-owned original wire metadata: defined hidden or deprecated original values remain observable, no-default values stay absent until configured, `rpc-secret` is withheld, and Rust-only uTP fields cannot leak into an original-client response. `changeUri` now honors the optional zero-based insertion position after deletions, matching the original ordering and count result; task-creation positions share the same rejection rules for negative values. `tellStatus`, `tellActive`, `tellWaiting`, and `tellStopped` honor the original optional `keys` field filter while preserving full output when omitted, and waiting/stopped pagination supports the original negative-offset semantics. Full original-client interoperability, including the browser-extension matrix and complete XML-RPC client coverage, remains unverified. |
| CLI and options | aria2/src/app/, aria2-core/src/config/ | PARTIAL | `OptionDef::parse_value` remains the shared typed seam for CLI/config/RPC validation, and `App::load_cli_args` now propagates validation failures instead of silently discarding them. Regression coverage proves invalid `--split=0` and unknown `--file-allocation` values are rejected before engine startup; startup coverage proves `--no-conf` skips an explicit config file as in the original. `IntegerRange` preserves ordered range wire values, `IndexOut` uses one cumulative `INDEX=PATH` parser for validation and BT execution, and `bt-prioritize-piece` validates the original `head[=SIZE],tail[=SIZE]` grammar through the same registry seam. The original short-option contract and the eight Rust additive aliases are covered separately, including `-h`/`-v`/`-V` actions. `-h`/`--help[=TAG|KEYWORD]` now preserves the optional-argument/getopt boundary, renders before engine startup, and filters by long-option keyword or supported help groups. The Rust-owned inventory baseline is `214` public names; the all-features registry has 22 additional Rust extensions, and the Rust-owned public-help baseline represents 198 names. Runtime changeability is now verified item-by-item against the Rust-owned compatibility baseline (`setInitialOption` 113/113, global 120/120, reserved 106/106, immediate 7/7), with Rust extensions listed separately. Exact defaults, help-tag membership/text, feature-conditional behavior, and full E2E proof remain open. CLI product identity and version output intentionally belong to `aria2-rust`. |
| Public C API/ABI | aria2_original/src/aria2api.cc, src/includes/aria2/aria2.h | PARTIAL | `aria2-core/src/c_api.rs` and `bindings/c/` provide a tested opaque-handle `extern "C"`/cdylib migration interface. The current host builds `aria2_core.dll` and an import library exporting all 19 declared `aria2_rust_*` functions; a temporary C consumer compiled, linked, and ran against that import library. It is intentionally source-level and is not binary-compatible with the original C++ classes or STL ABI; a platform ABI matrix and complete original `aria2api.h` semantic comparison remain open. |
| Bindings and SDKs | bindings/c/, bindings/python/, bindings/nodejs/ | PARTIAL | The C binding is covered through the core opaque-handle API; the maintained Python and Node.js clients have Rust-owned unit, integration, and local E2E suites. Current checkpoints report Python 137 passed and Node.js 123 passed with typecheck passing, but platform-specific binding runs, package publication metadata, and complete original-client interoperability remain open. |
| Examples and benchmarks | examples/, benches/, aria2-core/benches/, aria2-rpc/benches/ | PARTIAL | Example and benchmark targets are part of the Rust workspace inventory and compile in the workspace target checkpoint. Reproducible workload definitions and Rust-side performance regression results exist for selected seams, while complete benchmark coverage, allocation/lock/IO measurements, and a comparable aria2_original performance baseline remain open. |
| Tests and fixtures | aria2/tests/, aria2-core/tests/, aria2-protocol/tests/, aria2-rpc/tests/, tests/ | PARTIAL | Rust-owned fixtures, compatibility baselines, unit/integration/E2E tests, and binding suites are the only allowed regression inputs. Current targeted RPC tests and core target compilation pass; the complete cross-crate, platform, original-client, and browser-contract run remains open. |
| Workspace and CI | Cargo.toml, .github/workflows/ | PARTIAL | Workspace target compilation, formatting, and selected Clippy gates have reproducible evidence. CI release publishing and platform matrices are tracked as product-owned workflow behavior; current platform-specific runs, complete release validation, and one final aggregate acceptance run remain open. |

## Verification Evidence

### HTTP Authentication Checkpoint

The Rust HTTP paths now preserve the original Basic credential resolution
boundary while keeping the implementation independent: URL credentials take
priority, followed by explicit `http-user`/`http-passwd`, then matching
`.netrc` machine entries. These credentials are sent preemptively when the
resolver provides them. With
`http-auth-challenge=true`, requests without resolved credentials still wait
for the 401 challenge and retry once; an already-used explicit Authorization
header is not retried. This behavior is covered by the segmented Range
fixture and the `DownloadCommand` E2E
`engine_http_download_with_preemptive_auth`.

Digest remains an additive Rust capability behind the same challenge option.
Its parser accepts case-insensitive schemes, quoted commas, qop lists, and
the supported MD5/SHA-2 algorithms; both the Range and sequential
`DownloadCommand` E2E fixtures verify the server-side RFC response
calculation. NTLM and Negotiate intentionally fail as unsupported because the
original aria2 HTTP path does not implement those handshakes.

### Current Compatibility Slice (2026-08-14)

The HTTP request-policy slice now has production E2E evidence for default GET,
use-head, http-no-cache, no-want-digest-header, enable-http-keep-alive,
explicit header precedence, http-accept-gzip, and unknown-length chunked
responses. Every unknown-length path, including explicit split > 1, sends one
ordinary GET without a synthetic Range: bytes=0-0 probe and completes on one
connection. This matches aria2_original's `UnknownLengthPieceStorage` plus
`GrowSegment` behavior at the public download seam. The Rust implementation
currently realizes the same behavior through its sequential writer; wiring the
existing Rust unknown-length storage types into production is an internal
architecture cleanup, not a missing segmented-download feature.

The proxied production HTTP client also disables reqwest auto-redirects, so
direct, DNS-pinned, and proxied downloads all route redirects through the same
manual download-flow seam. A local proxy regression proves a 302 is returned
to that seam instead of being followed inside the transport client; this keeps
URI tracking, cookies, retry counts, and HTTP error mapping consistent.

The concurrent HTTP Range adapter now uses its own bounded manual redirect
seam because each segment is an independent request. Both buffered and
streaming range tests prove that a 302 is followed before Content-Range
validation, and a cookie regression proves that `Set-Cookie` on the redirect
response is stored and sent to the final request. The production concurrent
fixture reports 3 passed tests. Concurrent 401/407 responses are mapped to
the original HTTP authentication result code. The segmented request path now
uses the shared challenge credential resolution and retry seam for 401/407;
the sequential path also preserves proxy-URL credentials through a real 407
fallback fixture. Complete original-client interoperability coverage remains
open.

All production HTTP client construction paths audited in this slice make gzip
negotiation explicit: disabled by default and enabled only for the configured
download option. This includes the standalone protocol HTTP client used by
tracker code, Metalink, concurrent downloads, web seeds, and tracker clients.

The project emits one product identity. The Cargo package version
(`aria2-rust` 0.3.0 on this checkout), CLI version action and startup banner,
RPC `getVersion`,
default HTTP/tracker User-Agent, BitTorrent peer agent, and BitTorrent
extension handshake use the same release source.

The upstream C++ version-report text is not part of this product. It has been
removed because it would falsely claim the upstream implementation and linked
libraries. Compatibility is provided by the CLI entry point, option names,
RPC wire shapes, and protocol behaviour; product version values remain owned
by `aria2-rust`.

The public tracker catalog is product-owned Rust functionality. It is not an
aria2_original configuration contract and is documented as an additive
extension. When enabled, it contributes only extra public tracker announce
tiers; original tracker options and requests keep their original meaning. Its
Rust-only controls remain accepted through the Rust configuration/RPC input
seam, but are excluded from the original `getGlobalOption` and task
`getOption` projections so an original client does not receive extension
fields in a standard compatibility response. The catalog keeps its snapshot
while temporarily excluding unhealthy trackers, and a successful announce
clears the tracker backoff so the entry can recover.

The executable name `aria2c` and the RPC method/field names remain only as
compatibility entry points. They do not change the product identity or cause
the implementation to report an upstream aria2 version.

The CLI help and completion usage also retain `aria2c` for existing scripts,
while `--version` reports the independent `aria2-rust` product name and
workspace version.

The hidden optimization options also preserve the original case-sensitive
configuration contract: `optimize-concurrent-downloads-coeffA` defaults to
`5.0`, and `optimize-concurrent-downloads-coeffB` defaults to `25.0`.
Lowercase spellings are not aliases. This is an external option compatibility
fix; it does not change the independent `aria2-rust` product identity or
version policy.

The workspace is also version-consistent: all four Rust member crates use the
workspace package version, all internal path dependency constraints target
0.3.0, distribution manifests and SDK package metadata use 0.3.0, and active
installer fallbacks and examples use `aria2-rust/0.3.0`. Code-generated output
and test fixtures must not emit an upstream aria2 product version. External
input fixtures use neutral client or generator labels when a version field is
needed. Wire-protocol versions such as JSON-RPC `2.0`, Metalink `3.0`/`4.0`,
and SFTP version `3` are format versions, not product identity.

- `user-agent` and `peer-agent` registry defaults use `aria2-rust/0.3.0`.
  The peer-ID prefix is `A2-RUST-`; the default peer ID is generated once per
  process and remains exactly 20 bytes.
- RPC message shapes, method names, authentication, errors, notifications,
  routes, and protocol semantics remain the compatibility surface. The product
  identity values above are intentional, documented differences.
- `aria2.forceUnpause` is rejected as an unknown original RPC method and is
  not advertised. The Rust-only root HTTP information endpoint and `/ws`
  WebSocket alias were removed; `/` and `/ws` return 404, while `/jsonrpc`
  remains the compatible JSONP/JSON-RPC and WebSocket route.

This slice was checked with `cargo fmt --all -- --check`, the focused
`identity::tests::product_identity_uses_the_package_version`,
`bittorrent::peer::id::tests::test_generate_peer_id`, and
`handlers::handler_tests::test_get_version_uses_product_version` regressions,
the complete `test_cli_options` target (105 passed),
`cargo build -p aria2 --all-features -j 1` with an observed
`aria2c --version` value of `aria2-rust 0.3.0`, and
`cargo clippy -p aria2 --all-targets --all-features -- -D warnings`.
The RPC catalog additionally passed the independent
`handlers::system::tests::test_list_methods` regression and the real HTTP
`e2e_system_list_methods_returns_array` and
`e2e_force_unpause_is_rejected_as_an_unknown_original_method` tests.
The complete `test_e2e_all_rpc_methods` target passed 55 tests.
The complete `test_e2e_http_server` target passed 46 tests, covering JSON-RPC,
JSONP, XML-RPC, WebSocket, authentication, CORS, endpoint routing, and
compatible HTTP error contracts.
The `aria2` all-targets suite also passed, with 3 pre-existing daemon tests
ignored by design.
It does not establish complete original-client or end-to-end download
compatibility.

### Write-back cache correctness checkpoint (2026-08-14)

The Rust-native `WrDiskCache` now keeps cached ranges disjoint. A newer
overlapping write preserves the older range's untouched left and right
fragments, so reads spanning fragments assemble the same last-write-wins byte
sequence that `CachedDiskWriter` will persist. Flushes are serialized with
cache mutations through a small async gate; a stale flush snapshot therefore
cannot overwrite a newer concurrent write. Single-range reads remain
zero-copy, while only multi-fragment reads allocate a contiguous result.

Focused evidence:

~~~text
cargo test -p aria2-core --lib filesystem::disk_cache --all-features -- --test-threads=1
  20 passed, 0 failed
cargo clippy -p aria2-core --lib --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

This closes the cache overlap/flush correctness slice only. Piece/segment
aggregation remains an internal architectural difference, and the complete
BitTorrent scheduler, cross-protocol, original-client, workspace E2E, and
performance gates remain open.

### BitTorrent MSE and Shared Listener Slice (2026-08-13)

The incoming BitTorrent path now uses one process-level TCP listener and an
info-hash route registry. A route handle unregisters its torrent on drop, and
listener shutdown releases the socket. MSE route discovery verifies
`req2 ^ req3` before applying the torrent policy, so the Rust implementation
does not expose internal download state through the wire protocol.

The MSE handshake keeps the upstream wire boundary while using Rust-owned
state machines internally. DH public-key padding, VC look-ahead, RC4 stream
positions, PadA/PadB/PadC/PadD, and the post-handshake 68-byte BitTorrent
handshake are covered. The plaintext policy is also explicit: the MSE
negotiation response remains RC4-protected, then the negotiated post-handshake
stream is plaintext; `bt-force-encryption` selects RC4 and rejects legacy
plaintext, while `bt-require-crypto` rejects legacy plaintext without forcing a
different MSE method.

Focused evidence on 2026-08-13:

~~~text
cargo test -p aria2-protocol --features bittorrent mse_handshake --lib -- --test-threads=1
  18 passed, 0 failed
cargo test -p aria2-protocol --features bittorrent incoming::tests --lib -- --test-threads=1
  3 passed, 0 failed
cargo test -p aria2-core --features bittorrent bt_peer_listener::tests --lib -- --test-threads=1
  3 passed, 0 failed
cargo check -p aria2-protocol --features bittorrent
  PASS
cargo check -p aria2-core --features bittorrent
  PASS
~~~

The overall migration remains `PARTIAL`; these focused results do not prove
the complete CLI/RPC/client matrix, full BitTorrent scheduler and seeding
parity, workspace all-target E2E completion, or the aria2 C performance
comparison.

### BitTorrent context identity checkpoint (2026-08-13)

BitTorrent command promotion keeps an externally prepared `DownloadContext`
only when its Rust torrent attribute carries the same info-hash as the current
torrent bytes. A dependency context with a matching hash retains Metalink
output paths, URI mappings, and mirror settings. A missing or mismatched hash
causes the context to be rebuilt from the current torrent, preventing stale
piece hashes or output mappings from crossing session or dependency reuse.
This is an internal Rust ownership invariant behind the compatible torrent
entry point; it does not change public configuration, defaults, or product
identity.

Focused verification on 2026-08-13:

~~~text
cargo test -p aria2-core --features bittorrent bt_download_command_tests --lib -- --test-threads=1
  37 passed, 0 failed
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the stale-context regression slice only. BitTorrent remains
`PARTIAL` until scheduler/seeding parity, dependency/session graph coverage,
and original-client interoperability have complete evidence.

### Product Identity, Defaults, and Feature Wiring (2026-08-13)

`aria2-rust` is an independent product. The workspace and all public product
identity adapters use version `0.3.0`: CLI `--version`, the startup banner,
RPC `aria2.getVersion`, HTTP User-Agent, BitTorrent peer agent, SDK metadata,
and distribution metadata. The upstream C++ version-report text is not used.
JSON-RPC `2.0`, Metalink `3.0`/`4.0`, and SFTP `3` are protocol-format
versions, not product identity values.

The public `aria2-rust` defaults for `split` and
`max-connection-per-server` are intentionally Rust-owned values of 16 and 16.
`split` accepts values through 128 in this implementation. These defaults are
tracked as a compatibility difference rather than presented as
`aria2_original` defaults; request executor task count and transport idle-pool
size remain internal implementation details.

The BitTorrent seeding defaults also preserve the original `NO_DEFAULT_VALUE`
semantics for `seed-time`: an omitted option remains absent, while an explicit
`seed-time=0` is retained and disables the time criterion. `seed-ratio` remains
`1.0`, and either configured seeding criterion can terminate seeding once it is
satisfied.

Task construction uses one `CommandDependencies` value. Under the combined
BitTorrent/Metalink feature set, Metalink commands retain the shared
BitTorrent registry and listener, and torrent fallback forwards both into the
actual BitTorrent command. This preserves route registration and cleanup
without copying the original C++ ownership hierarchy.

The direct-command path is also covered: a standalone `BtDownloadCommand`
owns a Rust listener manager, while engine-created commands replace it with the
process-shared manager. Explicit `seed-time=0` takes precedence over the
default `seed-ratio=1.0`, matching the original option-defined criterion and
preventing an unintended infinite seeding phase.

Focused verification on 2026-08-13:

~~~text
test_cli_options --all-features: 105 passed, 0 failed
config::parser: 27 passed, 0 failed
metalink_download_command: 16 passed, 0 failed
aria2.getVersion product-version regression: passed
aria2 --all-features cargo check: PASS
aria2-core BitTorrent+Metalink Clippy (-D warnings): PASS
cargo fmt --all -- --check: PASS
cargo test -p aria2-core --test test_e2e_bittorrent_download --features bittorrent -- --test-threads=1: 21 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib bt_peer_listener::tests --features bittorrent -- --test-threads=1: 3 passed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings: PASS
~~~

This checkpoint does not change the overall `PARTIAL` status. Complete
original-client/browser-extension interoperability, workspace end-to-end
coverage, and the aria2 C performance comparison remain unverified.

### BitTorrent web-seed and integrity checkpoint (2026-08-13)

The command-level BitTorrent suite now has real local HTTP evidence for a
`url-list` web-seed when no peer is available. It also pauses a slow web-seed
transfer, verifies that the Rust-owned checkpoint is usable, resumes the
remaining pieces, and confirms successful cleanup. The `--check-integrity`
scenario starts with one valid and one corrupted piece; the integrity result
retains verified and failed piece indices, so the picker requests only the
failed piece and the final bitfield is complete.

~~~text
cargo test -p aria2-core --test test_e2e_bittorrent_download --all-features -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib checksum::check_integrity --all-features -- --test-threads=1
  9 passed, 0 failed
~~~

This closes the observed Rust-owned web-seed and BitTorrent integrity slice.
The overall area remains `PARTIAL`: complete scheduler/seeding parity, live
aria2_original client interoperability, browser-extension coverage, and the
workspace acceptance suite are still open.

### BitTorrent complete-integrity controls checkpoint (2026-08-14)

The Rust command now keeps the original external behavior for a complete
payload that passes `check-integrity` while keeping the implementation
Rust-owned. `bt-enable-hook-after-hash-check` controls the BT completion hook
at the integrity-check seam. `bt-hash-check-seed` controls whether the command
continues into tracker/peer setup after that check; when it is `false`, the
already verified payload is finalized locally and no tracker announce is
started. The default `true` path continues through a real local tracker and
peer fixture. Completion notification is emitted once even when the command
continues into the seed lifecycle.

The defaults and option names remain the existing public contract. This slice
does not copy the original C++ class structure, alter user configuration, or
change the `aria2-rust 0.3.0` product identity.

~~~text
cargo test -p aria2-core --features bittorrent --test test_e2e_bittorrent_download -- --test-threads=1
  28 passed, 0 failed, 2 ignored
~~~

This closes the observed complete-payload hash-check control slice only.
BitTorrent remains `PARTIAL` until scheduler/seeding parity, dependency and
session coverage, original-client interoperability, and workspace acceptance
gates are reproducibly green.

The same command target now includes a real multi-file integrity regression:
the first piece crosses two physical files, one byte in that logical piece is
corrupted, and only that piece is requested again. This verifies the existing
Rust `MultiFileChunkValidator` production seam instead of treating the
multi-file path as a single concatenated output file.

### SFTP checksum lifecycle checkpoint (2026-08-14)

The Rust SFTP command now uses the shared Rust checksum seam for both sides of
the lifecycle: a fresh or resumed transfer is verified after the writer is
finalized, and an existing local file whose length already equals the remote
length is verified before opening the remote data handle. A matching complete
file finishes without issuing SFTP `READ` requests. A mismatch resets the
resume offset and returns to the remote transfer path; completion and
checkpoint removal occur only after the replacement bytes pass verification.

This preserves the original `ChecksumCheckIntegrityEntry` observable behavior
without copying the C++ command hierarchy or changing the public
`aria2-rust 0.3.0` identity.

~~~text
cargo test -p aria2-core --features sftp --test test_e2e_sftp_download -- --test-threads=1
  12 passed, 0 failed
~~~

This closes the tested SFTP whole-file checksum lifecycle slice only. SFTP
remains `PARTIAL` until third-party server, public-key authentication,
complete original-client interoperability, and workspace acceptance coverage
are reproducibly green.

### Rust-owned cleanup validation (2026-08-14)

The obsolete FTP `file_preparation` copy layer was removed after a workspace
reference audit found no production callers. FTP runtime behavior remains in
the Rust async command path, and the removal changes no option, default,
session, RPC, CLI, or protocol wire contract.

~~~text
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3318 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo check -p aria2 --all-features
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

Verified on 2026-08-09 with single-job builds where needed:

~~~text
cargo fmt --all -- --check                              PASS
cargo check -p aria2-core --all-features --tests -j 1    PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings PASS
cargo clippy -p aria2-protocol --all-targets --all-features -- -D warnings PASS
cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings PASS
cargo test -p aria2-core --test test_e2e_download --all-features -- --test-threads=1 26 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib --all-features -- --test-threads=1 3307 passed, 1 ignored, 0 failed
cargo clippy --workspace --all-targets --all-features -- -D warnings PASS (prior checkpoint; not rerun in this incremental checkpoint)
cargo test -p aria2-core --all-features c_api --lib   PASS
cargo test -p aria2-protocol --all-features -j 1        872 passed, 4 ignored
aria2-rpc all-feature test targets (listed in Latest RPC Wire Checkpoint) 402 passed, 0 failed
cargo test -p aria2 --lib application_rpc_does_not_enable_cors_by_default --all-features -- --test-threads=1 1 passed, 0 failed
cargo test -p aria2 --all-features --tests -j 1 -- --test-threads=1 292 passed, 3 ignored, 0 failed
cargo build -p aria2 --all-features -j 1                PASS
npm run typecheck                                        PASS
npm run build                                            PASS
ARIA2_RUST_BIN=target/debug/aria2c.exe npm test          123 passed
PYTHONPATH=.codex-python-deps python -m pytest -p no:cacheprovider 137 passed
~~~

Latest focused verification on 2026-08-12:

~~~text
cargo test -p aria2-core --lib session::session_entry --all-features -j 1          19 passed
cargo test -p aria2-core --lib request::request_group::options --all-features -j 1  5 passed
cargo test -p aria2 --test test_cli_options --all-features -j 1                  105 passed
cargo test -p aria2 --lib app::tests --all-features -j 1                         20 passed
aria2-rpc all-feature targets (9 individual targets), before version update     402 passed
cargo test -p aria2-rpc --lib handlers::handler_tests::test_get_global_option_uses_original_wire_visibility_not_help_visibility --all-features -- --exact  1 passed
cargo test -p aria2-rpc --test test_e2e_all_rpc_methods --all-features -j 1       55 passed
cargo test -p aria2-rpc --test test_e2e_http_server --all-features -j 1           46 passed
cargo test -p aria2-rpc --test test_https_rpc --all-features -j 1                  4 passed
cargo metadata --no-deps --format-version 1                                      0.3.0 for all Rust members
cargo fmt --all -- --check                                                       PASS
~~~

Latest FTP/FTPS lifecycle checkpoint (2026-08-13):

~~~text
cargo check -p aria2-core --all-features --tests -j 1                         PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings        PASS
cargo fmt --all -- --check                                                     PASS
cargo test -p aria2-core --lib --all-features -j 1                              3342 passed, 0 failed, 1 ignored
cargo test -p aria2-core --lib engine::ftp_download_command --all-features -- --test-threads=1
  18 passed, 0 failed
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  31 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test test_e2e_sftp_download --all-features -- --test-threads=1
  18 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

The FTPS negative regression proves that an `ftps://` request does not accept
a plaintext FTP server. A positive explicit/implicit FTPS server exchange is
still unverified because the current fixture set has no valid reusable server
certificate/key pair. FTPS remains an additive Rust extension and is not an
original-aria2 compatibility requirement.

The production `FtpDownloadCommand` now follows the original PASV connection
target rule: it validates and logs the advertised host, but opens the data
socket to the control connection's peer address. The regression fixture
advertises `127.0.0.2` while listening on `127.0.0.1`; the download succeeds,
matching `aria2_original/src/FtpNegotiationCommand.cc` and avoiding failures
with NATed or misconfigured FTP servers. This is local protocol evidence, not
complete live-server or original-client interoperability evidence.

Fresh and pooled active-mode negotiation now bind the data listener to the
control connection's local IP, matching
`aria2_original/src/FtpConnection.cc::createServerSocket`. This keeps EPRT/PORT
advertisements on the selected interface for multi-homed hosts. The focused
negotiation target reports 39 passed tests; the production active-only E2E now
passes, while third-party server interoperability and multi-homed process
coverage remain open.

The FTP production path also verifies a configured whole-file `checksum` before
short-circuiting an existing same-length output, restarts from byte zero after
a mismatch, verifies newly received data through the same shared checksum
helper used by HTTP, and rejects a short `RETR` stream when `SIZE` reported a
known length. These checks close the focused FTP integrity gaps, but do not yet
prove piece-hash integrity-entry scheduling or third-party server
interoperability.

The FTP and SFTP command lifecycles now observe `Removed` and pause requests
inside the transfer loop. On pause or removal they close the remote handle,
finalize the local writer, and force an `A2CF` checkpoint save; unpause creates
a fresh command that resumes from the persisted prefix. Successful completion
removes the checkpoint. These are Rust-owned lifecycle and persistence details:
the `.aria2` suffix is retained for user familiarity, but `A2CF` is not claimed
to be binary-compatible with an aria2_original sidecar.

Latest retry-contract checkpoint (2026-08-09):

~~~text
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings       PASS
cargo test -p aria2-core --test test_retry --all-features -- --test-threads=1
  15 passed, 0 failed
cargo test -p aria2-core --test test_e2e_retry_contract --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib ftp --all-features -- --test-threads=1
  200 passed, 0 failed, 1 ignored
~~~

The retry policy matches `aria2_original`: default `max-tries=5`, the value is
the total number of attempts, and `max-tries=0` is unlimited. This checkpoint
is limited to retry behavior and does not change the overall HTTP/FTP or
workspace status, nor does it establish original-client interoperability.

Latest browser-facing RPC HTTP checkpoint (2026-08-09):

~~~text
cargo test -p aria2-rpc --test test_e2e_http_server --all-features -- --test-threads=1
  43 passed, 0 failed
cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings
  PASS
~~~

The OPTIONS preflight response now carries the same `Access-Control-Max-Age:
1728000` value as `aria2_original/src/HttpServerBodyCommand.cc`. This verifies
the browser-facing header at the HTTP seam. The application startup seam also
verifies that the default registry leaves `rpc-cors-domain` unset, so no
`Access-Control-Allow-Origin` header is emitted unless CORS is explicitly
configured. This does not establish the complete original browser-extension or
client interoperability matrix.

Focused HTTP resume regression evidence (2026-08-09):

~~~text
cargo test -p aria2-core --test test_e2e_download resume_failure --all-features -- --test-threads=1  4 passed, 0 failed
cargo test -p aria2-core --lib request::request_group::options::tests::rpc_option_map_uses_aria2_wire_strings --all-features -- --exact  PASS
~~~

Latest option/BitTorrent/port-range checkpoint (2026-08-09):

~~~text
cargo test -p aria2-core --lib config::option::tests --all-features -- --test-threads=1 34 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_download_command_tests --all-features -- --test-threads=1 34 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_peer_listener::tests --all-features -- --test-threads=1 4 passed, 0 failed
cargo test -p aria2-protocol --lib bittorrent::dht::engine::tests --all-features -- --test-threads=1 6 passed, 0 failed
cargo fmt --all -- --check PASS
~~~

This checkpoint verifies the shared `OptionDef`/RPC validation seam, cumulative
`index-out` parsing and actual BitTorrent output-path application for single and
multi-file torrents. It also verifies ordered TCP and UDP port-range fallback
when the first candidate is occupied. These are focused compatibility results;
they do not establish browser-extension interoperability or workspace-wide
end-to-end completion.

The current runtime policy seam additionally verifies that every name in the
global-changeable, initial-request, immediate-changeable, and reserved-change
sets resolves to the canonical `OptionRegistry` without duplicates:
`runtime_policy_names_are_registered_without_duplicates` passed, and
`cargo clippy -p aria2-core --all-targets --all-features -- -D warnings`
passed.

The Rust-owned compatibility baseline also reports no missing names in any of
those four lifecycle sets:
initial request options `113/113`, global-changeable options `120/120`,
reserved-change options `106/106`, and immediate-change options `7/7`.
Rust-only public-tracker controls are intentionally outside the original
initial-request set and outside the standard `getGlobalOption`/`getOption`
projections, while remaining accepted as explicit Rust extension inputs.

Latest BitTorrent piece-priority checkpoint (2026-08-12):

~~~text
cargo check -p aria2-core --all-features -j 1                         PASS
cargo test -p aria2-protocol --lib bittorrent::piece::picker --all-features -- --test-threads=1
  37 passed, 0 failed
cargo test -p aria2-core --lib config::option --all-features -- --test-threads=1
  40 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_piece_selector --all-features -- --test-threads=1
  13 passed, 0 failed
~~~

Compared with `aria2_original/src/OptionHandlerImpl.cc`,
`aria2_original/src/util.cc`, and `aria2_original/src/RequestGroup.cc`,
`bt-prioritize-piece` now has no synthetic `rarest` default, accepts only the
original `head[=SIZE],tail[=SIZE]` grammar with `K`/`M` units, computes the
file-boundary piece set from global file offsets, and tries that set before
the normal rarest-first picker. This closes the focused option/scheduler
semantic gap; full BitTorrent scheduler, seeding, and live interoperability
evidence remains open.

Latest CLI optional-argument checkpoint (2026-08-09):

~~~text
cargo test -p aria2 --test test_cli_options --all-features regression_h_does_not_set_listen_port -- --exact PASS
cargo test -p aria2 --test test_cli_options --all-features regression_help_selector -- --exact PASS
cargo test -p aria2 --test test_cli_options --all-features regression_help_rendering_filters_options -- --exact PASS
cargo check -p aria2 --all-features --tests -j 1 PASS
~~~

The CLI now accepts `-h`/`--help` with an optional value only in the
equals-attached form, preserving a following positional URI; the short
`-htimeout` form and the original `-h=timeout` truncation rule are covered as
well. `#tag` and option-name selectors are rendered before configuration or
engine startup.
The renderer still needs a generated comparison against the original option
inventory for exact tag membership, wording, and supported option semantics;
this checkpoint is parser and lifecycle evidence, not full CLI parity. Product
branding and version text are intentionally owned by `aria2-rust`.

The original public option inventory comparison on this checkout reports 198
public names in `aria2_original` and all 198 represented by the Rust CLI help;
the Rust surface currently has additional documented or compatibility-only
names. This proves name coverage only. It does not prove that every option's
runtime effect, default, changeability, hidden/deprecated status, or help text
matches the original.

Latest process-level RPC checkpoint (2026-08-09):

~~~text
cargo build -p aria2 --all-features -j 1                                      PASS
cargo test -p aria2 --lib app::rpc::bridge_tests --all-features -- --test-threads=1 7 passed, 0 failed
cargo test -p aria2 --test e2e_arianng_rpc_client -- --test-threads=1        1 passed, 0 failed
cargo clippy -p aria2 --test e2e_arianng_rpc_client -- -D warnings           PASS
cargo test -p aria2 --test e2e_arianng_rpc_client --all-features -- --test-threads=1 1 passed, 0 failed
cargo clippy -p aria2 --test e2e_arianng_rpc_client --all-features -- -D warnings PASS
~~~

The built `aria2c.exe` was started with `--enable-rpc=true` and no initial URI.
An original JSON-RPC envelope reached the live process: `aria2.getVersion`
returned the then-current package version, `system.listMethods` returned 36
methods including `aria2.addUri`, and `aria2.shutdown` returned
`OK. 0 active downloads paused.`
The process exited within five seconds. A separately occupied RPC port now
fails startup before the process-wide download-event bridge is registered.
The process-level AriaNg regression now starts a separate `aria2c` with
`rpc-secret`, sends the real `system.multicall` refresh shape with a `token:`
in every sub-call (and no envelope token), verifies the original nested result
wrappers and stringified global statistics, then shuts the process down through
`aria2.shutdown`. This is default and all-features evidence for the actual
CLI/listener/HTTP/auth/dispatch lifecycle. It does not yet prove the complete
Chrome/browser-extension matrix or all original-client workflows.

Latest RPC compatibility checkpoint (2026-08-09): unknown and
non-changeable options are ignored as in `aria2_original`; a recognized option
with an invalid value returns execution error `code=1` and HTTP 400 for
JSON-RPC. `addUri`, `addTorrent`, and `addMetalink` use the same core parser as
runtime option updates. Only registry-declared cumulative options accept RPC
arrays; ordinary options cannot be silently converted from arrays. The
checkpoint includes 224 library tests, 18 integration tests, 55 all-method
HTTP tests, 46 HTTP/WebSocket/XML-RPC route tests, 5 server-config tests, 4
HTTPS tests, 31 mock-server tests, 9 header/progress tests, and 10 stress
tests, for 402 passed and 0 failed. The aggregate command completed in the
current checkpoint. The
active/reserved task changeability policy is centralized with the global
policy in `aria2-core/src/config/runtime.rs`; `request_group` only preserves
the historical re-export path. This is a RPC-scope result only; it does not
establish workspace-wide completion or full compatibility with every original
browser client.

For live groups and stopped results, `aria2.getOption` now follows
`GetOptionRpcMethod::process()` directly: `RequestGroup` owns the creation
snapshot, core projects it through the original
`OptionHandlerFactory.cc::setInitialOption(true)` policy, and transfers the
effective state to `DownloadResult` during the terminal lifecycle transition.
This shares one rule across CLI-created tasks, RPC-created tasks, and
session-restored groups; it prevents listener/authentication settings and
`aria2-rust-*` session metadata from leaking through the task wire response.
Only task changes that have actually taken effect are overlaid, so a later
`changeGlobalOption` changes future groups but cannot rewrite an existing
group or stopped result. The header/progress and process-level
config-inheritance targets cover active-task behavior, and the stopped-task
process regression covers a CLI task after `forceRemove`, including an applied
`changeOption` override.

The RPC regression target also covers `aria2.changeUri`'s optional `pos`
parameter against `aria2_original/src/RpcMethodImpl.cc`: deletions happen
first, added URIs are inserted in input order at the requested zero-based
position, and the response remains `[delCount, addCount]`. The focused
position and legacy append tests both pass. This is method-level evidence and
does not change the overall RPC status from `PARTIAL`.

The status-query regression also covers the original optional `keys` argument:
an omitted or empty list keeps the complete status object, while a non-empty
list returns only requested known fields. The same shared filter is used by
`tellStatus`, `tellActive`, `tellWaiting`, and `tellStopped`; the focused
`tellStatus`/`tellWaiting` test passes, including unknown-key omission.
Pagination now requires the original offset/count arguments, accepts negative
offsets for reverse-from-end queries, and rejects negative counts with an
aria2 execution error. Task-creation position parsing uses the same optional
argument seam and rejects negative positions before registering a GID.

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
original endpoint. Explicitly empty `params=` is also omitted from the
generated request object, matching the original string builder. Basic Auth
follows the original empty-password rule: an empty `rpc-passwd` is treated as
username-only authentication. The new GET/JSONP and Basic Auth regressions
pass as part of the 46-route HTTP/WebSocket/XML-RPC target.

Single-response JSON-RPC errors also send `Connection: close`, matching
`aria2_original/src/HttpServerBodyCommand.cc` after `disableKeepAlive()`;
successful requests and batch responses retain the normal HTTP connection
reuse path. This is covered by
`e2e_jsonrpc_errors_close_the_http_connection_like_original`.

The XML-RPC adapter now also has source-backed HTTP contract coverage against
`aria2_original/src/HttpServerBodyCommand.cc`: parser or XML value conversion
failures return HTTP 400 with an empty body and no `Content-Type`, while a
successfully parsed method execution failure returns HTTP 200 with an XML
fault whose `faultCode` is `1`. The completed all-features RPC target set is
402 passed and 0 failed; the later default-visibility regression also passes
as a separate targeted test.

The XML-RPC value adapter was compared with
`aria2_original/src/XmlRpcRequestParserStateImpl.cc` and
`test/RpcHelperTest.cc`. Explicit XML strings now preserve leading and trailing
whitespace, and `<double>` request values are forwarded as strings, matching
the original parser's observable string-state behavior for `<double>`.
The focused `xml_rpc` target reports 15 passed and 0 failed; the full HTTP
target remains 46 passed and 0 failed. This is a parameter-coercion seam
regression, not complete XML-RPC client interoperability evidence.

The `getSessionInfo` value was compared with
`aria2_original/src/DownloadEngine.cc` and
`aria2_original/src/RpcMethodImpl.cc`. Rust generates the random key once when
`RpcEngine` is constructed and emits the same 40-character lowercase hex value
through JSON-RPC, XML-RPC, and WebSocket dispatch. The handler regression
asserts the exact length and alphabet as well as per-engine stability; the
legacy `rpc_helpers::generate_session_id` path now forwards to the canonical
`SessionInfo` implementation.

The session option adapter was compared with
`aria2_original/src/SessionSerializer.cc` and `OptionParser.cc`. It keeps
aria2's `load-cookies` key while accepting Rust's `cookie-file` input alias,
preserves ordered `listen-port`/`dht-listen-port` ranges, and restores the
non-default option set through the typed `DownloadOptions` parser. The focused
session-entry target reports 19 passed tests; this is session compatibility
evidence, not proof that every session lifecycle or original client workflow
is complete.

The `aria2.getServers` adapter now follows the original active-only contract:
it emits one file-index entry per active file and includes only requests that
currently have peer statistics. Configured mirrors are not emitted as fake
servers; waiting, paused, stopped, and unknown GIDs map to execution error
code 1 with HTTP 400 for JSON-RPC.

The current core command `cargo test -p aria2-core --all-features --tests --
--test-threads=1` completed with exit code 0. Its library target reported
3,440 passed and 1 ignored, and every integration, E2E, stress, and performance
target in that command passed. The aggregate workspace command
`cargo test --workspace --all-features -j 1` has not been used as a green gate,
so this document does not claim one workspace aggregate run.

The current remaining acceptance gaps are:

- The original C++ class/STL ABI is not and cannot be claimed as binary
  compatible; the Rust project currently provides a separate opaque-handle
  source-level C migration ABI. Header/API coverage beyond the current
  session, control, and snapshot surface is still incomplete.
- Metalink dependency lifecycle now has explicit metadata-success,
  direct-mirror-fallback, and terminal-failure states. Named same-metaurl
  multi-file grouping, the original grouping fixture, shared command fallback,
  and zero-length torrent completion have converter, manager-graph, and local
  HTTP regression evidence. Standard memory-backed session graph restoration
  is now covered at the application save/restart seam. `follow-torrent=mem`,
  other session graph variants, and real HTTP/FTP/SFTP/DHT/BitTorrent Metalink
  interoperability still need implementation or reproducible evidence.
- HTTP, FTP, and DHT still have multiple layers with incomplete canonical
  ownership; third-party SFTP plus live FTP/DHT interoperability remains
  unverified here. The local FTP/SFTP fixtures are reproducible evidence for
  protocol behavior and the tested pause/remove/unpause lifecycle only.
- HTTP sequential resume now distinguishes a Range request answered with 200:
  default `always-resume=true` returns `CannotResume`, while
  `always-resume=false` restarts from byte zero. Multi-URI failover and
  `max-resume-failure-tries` are covered for the sequential HTTP path. Local
  concurrent Range, gap, FTP, and SFTP retry fixtures also pass; the complete
  cross-protocol, third-party, and original-client retry matrix remains open.
- CLI/config defaults, error semantics, exact help-tag and
  help/version text parity, and the complete original-client matrix still need
  generated comparison and E2E proof. The optional-argument parser boundary is
  covered, but that does not establish complete CLI output parity.
- Some network-oriented tests remain intentionally ignored; ignored tests are
  not counted as compatibility evidence.
- No comparable aria2 C++ performance baseline has been recorded. Rust-only
  benchmark results are regression evidence, not proof of superiority.

## Latest SFTP Checkpoint

The local SFTP fixture uses a real SSH handshake and SFTP v3 channel, not a
mocked client transport. It establishes password authentication, original
SHA-1 host-key pinning, missing-file mapping, full download, local-prefix
resume, and the engine pause/remove/unpause lifecycle recorded in the matrix
above:

~~~text
cargo test -p aria2-protocol --lib --features sftp -- --test-threads=1
  137 passed, 0 failed
cargo test -p aria2-core --lib engine::sftp_download_command::tests --features sftp -- --test-threads=1
  16 passed, 0 failed
cargo test -p aria2-core --test test_e2e_sftp_download --all-features -- --test-threads=1
  18 passed, 0 failed, 2 ignored
cargo clippy -p aria2-protocol --lib --features sftp -- -D warnings
  PASS
cargo clippy -p aria2-core --lib --features sftp -- -D warnings
  PASS
cargo clippy -p aria2-core --test test_e2e_sftp_download --features sftp -- -D warnings
  PASS
~~~

The FTP suite has the corresponding real slow-server lifecycle evidence:

~~~text
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  29 passed, 0 failed, 2 ignored
~~~

Both suites use the Rust-owned `A2CF` checkpoint format behind the familiar
`.aria2` path. This is local-server compatibility evidence only; it does not establish
interoperability with OpenSSH or other third-party SFTP servers, public-key
authentication, or the complete original SFTP error and extension matrix.

## Latest RPC Wire Checkpoint

The XML-RPC value adapter was rechecked against
`aria2_original/src/XmlRpcRequestParserStateImpl.cc` and
`aria2_original/src/RpcResponse.cc`. Rust keeps its own typed parser, but now
matches the original state-machine boundary for malformed scalar values and
struct members: invalid or out-of-range integer values, unknown value nodes,
missing member names, empty member names, and missing member values are omitted
from the current frame instead of turning a well-formed XML document into an
HTTP 400 parser failure. A genuinely malformed XML document still returns HTTP
400 with an empty body. The Rust `array_val` serializer also emits one
XML-RPC `<param>` containing an array, matching the original response shape.
Standard Rust-side XML-RPC extensions (`boolean`, `i8`, `dateTime.iso8601`, and
`nil`) remain additive and do not alter the original `int`/`i4`, string,
double, base64, array, or struct paths.

The new focused coverage is in `aria2-rpc/src/xml_rpc.rs` and
`aria2-rpc/tests/test_e2e_http_server.rs`; it covers invalid scalar omission,
struct-member omission, empty implicit values, one-param array responses, and
the live HTTP 200 XML execution-fault contract for a parseable invalid integer.

The WebSocket adapter was compared with
`aria2_original/src/WebSocketSession.cc`: the original passes every
non-control frame, including binary frames, to its JSON-RPC parser and returns
text JSON-RPC responses. `aria2-rpc/src/server/ws_session.rs` now preserves
that behavior by dispatching text and binary payload bytes through the same
wire parser. The wildcard CORS route was also checked against
`HttpServer::feedResponse`: with `rpc-allow-origin-all=true`, a normal RPC
response emits `Access-Control-Allow-Origin: *` even without a request
`Origin` header. The existing Rust behavior already matched, so the new E2E
is regression evidence rather than a CORS implementation change.

The same original WebSocket source keeps `rpc-max-request-size` inside its
incremental JSON parser: an oversized non-control message yields JSON-RPC
`-32700` and the session remains usable. The Rust upgrade routes previously
applied this option as an Axum frame/message cap, which reset the socket before
an aria2 response could be sent. `/jsonrpc` passes the logical limit into the
shared parser adapter. A lower transport-level default
limit remains a Rust implementation safety ceiling; it is separate from the
aria2 option and does not affect the covered 1 KiB compatibility case.

For HTTP, `aria2_original` checks an authenticated non-WebSocket request's
declared `Content-Length` before scheduling its body parser. When it exceeds
`rpc-max-request-size`, the server silently drops the connection rather than
writing an HTTP or JSON-RPC error. The process E2E verifies the same Rust
behavior at 1 KiB and separately verifies that failed Basic authentication
still returns its original `401` challenge before this close path. The HTTPS
target uses the repository Rustls certificate fixture to make a real TLS
connection and complete `aria2.getVersion`; it is not only configuration
coverage.

All `aria2-rpc` all-feature test targets completed in the current-tree serial
aggregate command for 411 passed and 0 failed:
232 library, 18 integration, 55 all-method E2E, 47 HTTP/WebSocket/XML-RPC
route E2E, 5 server, 4 HTTPS, 31 mock-server, 9 header/progress, and 10 stress
tests. `cargo fmt --all -- --check` and
`cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings` also
passed.

Three process-level public-client tests now supplement the in-process route
suite. `aria2/tests/e2e_arianng_rpc_client.rs` verifies AriaNg's real
`system.multicall` refresh shape with per-subcall `token:` values, nested
results, stringified statistics, and graceful shutdown.
`aria2/tests/e2e_websocket_rpc_client.rs` verifies a live WebSocket
`/jsonrpc` client can authenticate, correlate request IDs, and receive
`aria2.onDownloadStart` and `aria2.onDownloadStop` notifications with the
same GID on one connection. It also verifies an oversized request returns
`-32700` without disconnecting the client, and that a cleanly closed client
can reconnect and receive a later `onDownloadStart` notification.
`aria2/tests/e2e_xmlrpc_client.rs` verifies the live XML-RPC `/rpc` adapter
accepts the leading `token:` parameter, returns `text/xml` method responses,
preserves string statistics, and shuts down cleanly. The latest current-tree
commands passed:

~~~text
cargo test -p aria2 --test e2e_arianng_rpc_client --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2 --test e2e_websocket_rpc_client --all-features -- --test-threads=1
  3 passed, 0 failed
cargo test -p aria2-rpc --test test_e2e_http_server --all-features -- --test-threads=1
  47 passed, 0 failed
cargo test -p aria2-rpc --lib --tests --all-features -- --test-threads=1
  411 passed, 0 failed
cargo test -p aria2-rpc --test test_https_rpc --all-features -- --test-threads=1
  4 passed, 0 failed
cargo test -p aria2-rpc --lib websocket::tests --all-features -- --test-threads=1
  27 passed, 0 failed
cargo test -p aria2 --test e2e_xmlrpc_client --all-features -- --test-threads=1
  1 passed, 0 failed
cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings
  PASS
cargo test -p aria2-core --lib config::option::registry::tests --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-rpc --lib handlers::handler_tests::test_get_global_option_uses_original_wire_visibility_not_help_visibility --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2 --test e2e_rpc_config_inheritance --all-features -j 1 -- --test-threads=1
  5 passed, 0 failed
cargo test -p aria2 --test e2e_rpc_request_limits --all-features -- --test-threads=1
  2 passed, 0 failed
cargo clippy -p aria2 --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

The config-inheritance regression starts a live `aria2c` five times: with a
CLI `--dir` value, with `aria2.changeGlobalOption({"dir": ...})`, with a
CLI-created active task followed by that global mutation, and with a
CLI-created task that is force-removed after an applied `changeOption`, plus
one process configured with the Rust-only `--enable-utp=true` option. The
first two prove that a later JSON-RPC `aria2.addUri` inherits the expected
directory and `tellStatus` exposes the corresponding full file path. The last
two task-lifecycle runs prove that `getOption` retains the CLI task's creation-time directory both
while active and after it is stored as a stopped result, including an already
applied task override. The fifth process proves that the public
`getGlobalOption` response includes the C++ hidden `dns-timeout=30`, does not
synthesize the C++ `NO_DEFAULT_VALUE` preference `enable-async-dns6`, reports
that preference after the original `changeGlobalOption` path explicitly
defines it, and does not expose either uTP extension name. Focused
registry/RPC tests additionally cover `rpc-secret` and an unregistered
internal key. This confirms one
canonical global state for new tasks and per-task state across the
live-to-stopped lifecycle; it does not establish every option's changeability,
default, or full original-client interoperability.

The notification publisher now gives each WebSocket connection a unique
scoped subscription. Its receiver and bookkeeping entry share one RAII
lifetime, so a normal disconnect or task cancellation removes the entry just
as `aria2_original` removes a `WebSocketSession`. Public non-WebSocket callers
keep the established explicit `subscribe` / `unsubscribe` interface. The
focused unit regression proves both event delivery and automatic cleanup;
the process regression proves one client can close, reconnect, and receive a
new lifecycle event.

This checkpoint strengthens source-backed and real-client wire compatibility
only. RPC and WebSocket remain `PARTIAL` until original browser extensions
and a broader original-client matrix, notification ordering and broader
reconnect behavior, complete XML-RPC method/error coverage, and the remaining
control-frame combinations have reproducible end-to-end evidence.

## Latest File Allocation Checkpoint

The process-wide file-allocation worker now retains its own `Arc` clone while
`shared()` returns the canonical manager to callers. This fixes the
all-features move-after-capture compile failure without changing queue or
worker lifecycle semantics. The following focused checks passed:

~~~text
cargo check -p aria2-core --all-features                                      PASS
cargo test -p aria2-core --lib filesystem::file_allocation_man::tests --all-features -- --test-threads=1
  16 passed, 0 failed
~~~

## Architecture And Duplication Register

The current refactoring uses these module seams and records the remaining
consolidation work explicitly:

| Concern | Canonical seam | Decision / next action |
| --- | --- | --- |
| Engine command creation | `aria2-core/src/engine/task_spawner.rs` | Keep protocol selection and construction behind this deep module; the engine loop only owns lifecycle accounting and admission. |
| Global bandwidth limits | `RateLimiter` shared through `Arc` | One token-bucket state is shared by active and future commands; RPC updates also refresh `RequestGroupMan`'s reporting snapshot. |
| WebSocket connection lifecycle | `EventPublisher::subscribe_scoped` / `ScopedSubscription` | Each live WebSocket connection receives a unique broadcast registration. Rust RAII removes its bookkeeping entry whenever the receiver task exits, while existing public callers retain explicit `subscribe` / `unsubscribe`. This matches the original session manager's add/remove lifecycle without making the wire adapters own shared publisher state. |
| DHT | `aria2-protocol/src/bittorrent/dht/` | Keep the protocol crate as the canonical implementation; do not revive the unexported duplicate core tree. |
| RPC/CLI option parsing | `aria2-core/src/config/option/registry.rs`, `config/option/types.rs`, `request/request_group/options.rs`, `config/runtime.rs`, and `request/request_group/options_ops.rs` | Keep original runtime changeability, initial-request eligibility, enum choices, typed string/size/integer/boolean parsing, and cumulative `index-out` parsing in core. `OptionRegistry::parse_rpc_value` validates transport values, `DownloadOptions::try_from_rpc_options` validates task creation, and `project_initial_options` encodes the original `setInitialOption(true)` contract for task snapshots; RPC handlers only normalize transport values and map parse failures to aria2 execution errors. BT execution consumes the shared `parse_index_out` result and applies it to its output-path views. Do not add another option whitelist or parser in the RPC crate; JSON-RPC, XML-RPC, WebSocket, CLI, and config adapters must still preserve their distinct original wire parsing, error, and connection behavior. |
| Request-group identity lookup | `aria2-core/src/request/request_group_man/mod.rs` | Keep `active` and `reserved` as scheduling stores, but use one canonical GID index for all non-terminal groups. Active/reserved movement does not remove the index; terminal demotion/removal does. RPC, C API, session snapshots, and status lookup therefore share one stable identity seam without exposing the internal storage choice. Query snapshots preserve active-first and reserved FIFO order, then append canonical-only groups observed during a transfer window. |
| Write-back cache | `aria2-core/src/filesystem/disk_cache/` and `filesystem/disk_writer/buffered.rs` | Keep the cache Rust-native and range-based. `WrDiskCache` normalizes overlapping writes into disjoint `Bytes` fragments, assembles cross-fragment reads, serializes mutation with external flush I/O, and drains pending ranges before large direct-write bypasses. Do not copy C++ piece/segment ownership; remaining work is broader production aggregation and error propagation coverage. |
| Resume policy | `aria2-core/src/engine/download_command/execute.rs` and `request/request_group/control_ops.rs` | Keep HTTP response interpretation in `SequentialDownloader`; command-level URI selection, atomic resume-failure accumulation, and fresh-download fallback stay in `DownloadCommand`. Do not make RPC or protocol adapters reinterpret `always-resume` or `max-resume-failure-tries`. |
| HTTP/FTP transport | core orchestration plus protocol transport | Existing layers are useful adapters but are not yet one canonical implementation; remove pass-through duplication only after behavior comparison and live interoperability coverage. |
| FTP ownership | `aria2-core/src/engine/ftp_download_command/` is the only production FTP/FTPS path; `aria2-core/src/ftp/connection/negotiation/` and `aria2-protocol/src/ftp/` have no core production callers | Treat the engine path as the current behavioral reference. The target is one deep core FTP transport seam, but `FtpClient`, `FtpNegotiator`, and the standalone protocol client must not be deleted or merged until their public Rust API and behavior are covered by replacement tests. |
| Integrity | `aria2-core/src/checksum/check_integrity/man.rs` plus streaming/control-file paths | Keep `CheckIntegrityTask`/`IntegrityOutcome` as the production seam; `FileChunkValidator` and `MultiFileChunkValidator` provide the concrete adapters. The legacy exported wrapper types are not production callers and require an explicit Rust-crate interface decision before removal. |

## Acceptance Gate

The external compatibility boundary is strict: every public surface supported
by `aria2_original` must match it, including CLI/configuration, RPC/JSON-RPC/
XML-RPC/WebSocket wire shapes, authentication, parameters, errors, HTTP status
codes, session files, notifications, and observable lifecycle behavior. This
is the compatibility requirement for existing original clients and browser
extensions. Internal Rust architecture and performance are free to improve
only behind that boundary; extensions are evaluated separately and must be
additive.

The migration is not complete until the matrix is backed by reproducible
tests for default, BitTorrent, Metalink, SFTP, RPC, CLI, session/resume, and
binding workflows on the supported platforms. A green focused test or a
completed source comparison must not be reported as workspace all pass.

Performance claims also require a recorded benchmark protocol and comparable
aria2 C measurements. Rust-only benchmark results are regression evidence,
not proof of outperforming the original.

## BitTorrent DHT Periodic Lookup Checkpoint (2026-08-14)

The periodic DHT lookup preserves the original command's two distinct peer
observations. Active connections select the adaptive interval, while the
tracked peer count includes both queued and connected peers and drives the
retry/max-peer decision. Results are collected without blocking the piece loop,
then admitted through the existing connection and `PeerStorage` seam before
retry state is committed. Normal command shutdown cancels and joins a pending
lookup; `Drop` supplies only the synchronous fallback.

Focused evidence:

~~~text
cargo test -p aria2-core --features bittorrent --lib engine::bt_download_execute::execute::dht_periodic_lookup::tests -- --test-threads=1
  12 passed, 0 failed
cargo test -p aria2-core --test dht_integration_tests --features bittorrent -- --test-threads=1
  30 passed, 0 failed, 4 ignored
cargo test -p aria2-core --test test_e2e_bittorrent_download --features bittorrent -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test deep_e2e_bittorrent --features bittorrent -- --test-threads=1
  31 passed, 0 failed, 2 ignored
cargo check -p aria2-core --features bittorrent --lib
  PASS
cargo fmt --all -- --check
  PASS
~~~

This is a local scheduling and lifecycle checkpoint, not a claim of complete
DHT compatibility. Public-network behavior, exact advertise-port lifecycle,
original-client interoperability, full BitTorrent scheduler/seeding parity,
and the workspace acceptance gates remain open. The area remains `PARTIAL`.

The protocol DHT engine also now owns its receive, periodic, and bootstrap task
handles. A shared shutdown signal, cooperative wait, bounded abort, and final
join make `shutdown_async()` deterministic; synchronous `shutdown()` exposes
`ShuttingDown` immediately. This is an internal Rust lifecycle improvement and
does not change configuration, product identity, or external DHT wire behavior.
The former unreferenced `aria2-core/src/dht/` duplicate was removed after a
source/dependency audit confirmed that production paths and tests use only the
protocol implementation.

Lifecycle evidence:

~~~text
cargo test -p aria2-protocol --features bittorrent --lib bittorrent::dht::engine::tests -- --test-threads=1
  7 passed, 0 failed
cargo test -p aria2-core --test dht_integration_tests --features bittorrent -- --test-threads=1
  30 passed, 0 failed, 4 ignored
cargo check -p aria2-core --features bittorrent --lib
  PASS
cargo clippy -p aria2-protocol --all-targets --features bittorrent -- -D warnings
  PASS
~~~

#### RPC multicall envelope compatibility checkpoint (2026-08-14)

`system.multicall` now matches the original envelope contract: parameter zero
is the sub-call list, the envelope itself does not consume a `token:` value,
and every protected sub-call must carry its own leading token. Missing or
mistyped envelope parameter zero is reported as execution error `code=1`.
A sub-call whose `params` member is missing or is not an array is normalized
to an empty positional list, matching the original `checkParam<List>` path.

This is a Rust-owned wire seam. No C++ RPC class hierarchy was copied, and no
CLI/configuration/default/product-version value was changed. The overall RPC
area remains `PARTIAL` until the complete original-client and browser
extension matrix is exercised.

Focused verification:

~~~text
cargo test -p aria2-rpc --test test_e2e_all_rpc_methods -- --test-threads=1
  53 passed, 0 failed
cargo test -p aria2-rpc --test test_e2e_http_server -- --test-threads=1
  47 passed, 0 failed
cargo test -p aria2-rpc --test integration_rpc -- --test-threads=1
  18 passed, 0 failed
~~~

### BitTorrent seeding lifecycle checkpoint (2026-08-14)

The Rust seeding lifecycle now follows the original command semantics without
copying the C++ command hierarchy. `seed-time` is converted from fractional
minutes to truncated whole seconds, matching `BtSetup.cc`. A completed
torrent enters the seeding loop even when it has no active peers; the loop
continues beside the shared listener and admits later plain or MSE-encrypted
incoming peers. Upload sessions retain their transport variant instead of
silently dropping encryption state, and the upload counter is cumulative when
a peer disconnects, so seed-ratio evaluation remains stable.

Focused evidence:

~~~text
cargo check -p aria2-core --lib --all-features
  PASS
cargo test -p aria2-core --lib bt_seed_manager --features bittorrent
  2 new regression tests passed
cargo test -p aria2-core --test test_e2e_bt_seeding --features bittorrent
  10 passed, 0 failed
cargo test -p aria2-core --lib bt_download_command --features bittorrent
  41 passed, 0 failed
cargo test -p aria2-protocol --lib encrypted_connection --features bittorrent
  3 passed, 0 failed
cargo test -p aria2-core --test test_e2e_bittorrent_download --features bittorrent
  28 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test deep_e2e_bittorrent --features bittorrent
  31 passed, 0 failed, 2 ignored
cargo fmt --all -- --check
  PASS
~~~

This checkpoint does not change configuration definitions, defaults, product
identity, or external RPC/CLI contracts. BitTorrent remains `PARTIAL` until
full scheduler and seed behavior, original-client/browser interoperability,
and workspace end-to-end gates have reproducible evidence.

### FTP control-response parsing checkpoint (2026-08-14)

The shared Rust FTP control-response seam now distinguishes the same two
failure classes required by `aria2_original/src/FtpConnection.cc`: EOF or a
truncated CRLF-terminated response is a temporary network failure, while an
invalid first status line is an FTP protocol error. Complete single-line and
multiline responses retain the Rust-owned message representation used by the
fresh, pooled, and post-transfer control adapters. This is a focused parser
hardening change; it does not copy the C++ state machine or change any
configuration, default, product identity, CLI, RPC, or protocol wire value.

Focused verification:

~~~text
cargo test -p aria2-core --lib ftp::connection::negotiation --all-features -- --test-threads=1
  37 passed, 0 failed
cargo test -p aria2-core --lib engine::ftp_download_command --all-features -- --test-threads=1
  20 passed, 0 failed
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  31 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test ftp_integration_test --all-features -- --test-threads=1
  13 passed, 0 failed
cargo clippy -p aria2-core --lib --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

FTP remains `PARTIAL`: third-party server coverage, multi-homed process
coverage, FTPS positive interoperability, and the complete original-client
matrix are still open.

The production FTP control adapter also now rejects a `213 SIZE` value above
the signed file-offset range before progress, allocation, or resume state can
consume it. This follows `FtpNegotiationCommand::recvSize()` and remains an
internal Rust validation; no option, default, or public wire value changes.

The same shared address policy is now used by the production
`engine/ftp_download_command` active-mode path as well as the standalone
negotiation adapter. Both bind from the control connection's local IP before
issuing EPRT/PORT. The active-only fixture now rejects PASV and verifies a
real server-to-client data connection through the production engine.
Third-party active-mode interoperability and multi-homed process coverage
remain open.

### FTP proxy production-path checkpoint (2026-08-14)

The existing `ftp-proxy`, `ftp-proxy-user`, `ftp-proxy-passwd`,
`all-proxy`, `all-proxy-user`, `all-proxy-passwd`, `proxy-method`, and
`no-proxy` options are now consumed by the Rust FTP production command. The
default `proxy-method=get` path sends an absolute `ftp://` request target to
an HTTP forward proxy and streams the parsed HTTP response through the Rust
disk-writer/checkpoint/rate-limit/checksum lifecycle. The explicit
`proxy-method=tunnel` path establishes an HTTP CONNECT tunnel and then uses
the existing Rust FTP/FTPS control and data negotiation. Proxy credentials
keep the existing protocol-specific-over-all precedence, and tunnel mode no
longer resolves the origin locally when the proxy can resolve it.

This is an internal Rust implementation. No option name, default, user
configuration format, CLI/RPC wire value, or `aria2-rust 0.3.0` product
identity changed; no C++ proxy command chain was copied into the repository.

Focused verification:

~~~text
cargo test -p aria2-core --test test_e2e_ftp_proxy --all-features -- --test-threads=1
  12 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  35 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib ftp::connection::negotiation --all-features -- --test-threads=1
  39 passed, 0 failed
~~~

The complete original-client FTP proxy matrix, third-party forward-proxy
implementations, proxy redirect/chunked-response behavior, proxy FTPS
interoperability, and workspace acceptance remain open. FTP and the overall
migration remain `PARTIAL`.

### Multi-file preallocation checkpoint (2026-08-14)

`MultiFileAllocationIterator` and the production `FileAllocationMan` now use
the same Rust-owned allocation policy: `prealloc` probes native fallocate and
falls back to cooperative zero-fill, while `falloc`, `trunc`, and `none` keep
their distinct semantics. `secure-falloc` reaches the adaptive and native
paths, and fallback/security zero-fill starts at the existing file length so
resume data is preserved. This changes no option name, default, configuration
format, product version, or public wire contract.

The public Rust allocation helper now uses that same native `prealloc` path;
it no longer silently degrades `prealloc` to `set_len`. Existing file prefixes
are covered by a regression test at the public entry point.

Focused verification:

~~~text
cargo test -p aria2-core filesystem::file_allocation --lib
  43 passed, 0 failed
cargo test -p aria2-core filesystem::file_allocation_man --lib
  16 passed, 0 failed
cargo test -p aria2-core multi_file_allocation_iterator --lib
  3 passed, 0 failed
~~~

The broader filesystem matrix, platform-specific allocation behavior on
macOS/Windows, and workspace migration acceptance remain `PARTIAL`.

## BitTorrent Selective Files And Completion Cleanup Checkpoint (2026-08-15)

The Rust BitTorrent command now keeps `select-file` at the existing external
option seam while applying it in the Rust-owned torrent context and piece
picker. File indices remain 1-based on the wire; selected file byte ranges are
translated to global piece indices, so a piece shared by a selected and an
unselected file is still downloaded and verified as one torrent piece. The
picker's allowed set is consulted by all local selection strategies,
priorities, endgame candidates, Suggest messages, and selective completion
counts. The persisted checkpoint bitfield remains global so resume state is
not rewritten into a Rust-only filtered format.

The existing `bt-remove-unselected-file` option is now carried by
`DownloadOptions`, RPC runtime updates, and session serialization. The BT
finalization seam mirrors the original observable conditions: cleanup runs
only after successful completion, for a BitTorrent context, on disk-backed
downloads, and only for entries marked unrequested. Failed or missing file
removals are warnings and do not turn an already completed download into a
failed task. No option name, default, user configuration, product version, or
upstream version-report text was changed; the implementation remains
Rust-native.

Focused verification:

~~~text
cargo fmt --all -- --check                                           PASS
cargo test -p aria2-core --features bittorrent --lib engine::bt_download_execute::execute::finalization::tests -- --nocapture
  2 passed, 0 failed
cargo test -p aria2-core --features bittorrent --lib request::request_group::options::tests -- --nocapture
  10 passed, 0 failed
cargo test -p aria2-core --features bittorrent --lib session::session_entry::tests::test_download_options_to_map_all_fields -- --nocapture
  1 passed, 0 failed
cargo test -p aria2-core --features bittorrent --test test_e2e_bittorrent_download test_e2e_bt_select_file_downloads_cross_boundary_piece_and_removes_unselected_file -- --exact --test-threads=1 --nocapture
  1 passed, 0 failed
cargo test -p aria2-core --lib segment::piece_storage --all-features -- --test-threads=1
  73 passed, 0 failed
cargo test -p aria2-core --features bittorrent --test test_e2e_bittorrent_download -- --test-threads=1
  29 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3390 passed, 0 failed, 1 ignored
cargo test -p aria2-rpc --all-features --lib -- --test-threads=1
  232 passed, 0 failed
cargo test -p aria2-protocol --all-features --lib -- --test-threads=1
  831 passed, 0 failed
cargo clippy -p aria2-core -p aria2-rpc -p aria2-protocol --all-targets --all-features -- -D warnings
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes the focused selective-file, checkpoint resume/web-seed, failed-peer,
and completion-cleanup slice only. The overall BitTorrent and workspace
migration remains `PARTIAL`: complete scheduler/seeding parity and
original-client/browser interoperability still require reproducible evidence.
The old context-free `PieceStorage` file-filter setup API was removed because
it had no production caller and duplicated the Rust-owned
`DownloadContext -> allowed_piece_indices -> PiecePicker` path. The underlying
`BitfieldMan` filter behavior remains covered for segment-storage consumers;
this internal cleanup changes no public option, default, session format, RPC
wire shape, or product identity.

### Disk read cache hint checkpoint (2026-08-14)

The Rust `DirectDiskAdaptor` and `MultiDiskAdaptor` now implement the
original `readDataDropCache` behavior through one internal cache-advice
helper. POSIX builds issue best-effort `posix_fadvise(DONTNEED)` for the
actual bytes read, including each segment of a cross-file read; non-POSIX
builds keep the operation as a no-op. Read data and error behavior are
unchanged.

Focused verification:

~~~text
cargo test -p aria2-core multi_disk_adaptor --lib --all-features
  44 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes the local cache-advice slice only; platform-specific filesystem
semantics and full workspace acceptance remain `PARTIAL`.
