# aria2 → Rust 迁移主台账

## 2026-08-17 Session 默认 seed-ratio 检查点

`DownloadOptions::default().seed_ratio` 为 `Some(1.0)`，与 `seed-ratio` 选项
定义一致。session 为保持文件紧凑性会省略这个默认值；恢复路径现在在键缺失
或值无效时恢复为同一个 typed 默认值。显式 `seed-ratio` 值以及
`seed-time=0` 仍保持独立语义。

Rust-owned 验证：

~~~text
cargo test -p aria2-core --all-features --lib defaults_excluded -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --lib seed -- --test-threads=1
  35 passed, 0 failed
cargo test -p aria2-core --all-features --lib --tests -j 1 --quiet -- --test-threads=1
  aria2-core library: 3470 passed, 0 failed, 1 ignored
  all aria2-core integration test targets passed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

本检查点只关闭 session option boundary 的 typed 默认值恢复切片；更广泛的
session 重启组合、跨协议生命周期、第三方/原版客户端互操作、bindings、实测
性能和最终 workspace 验收仍未完成。当前阶段保持 `phase-2-core-domain`，
整体迁移保持 `PARTIAL`。

## 2026-08-17 RequestGroup 调度公平性检查点

`RequestGroupMan::fill_from_reserver` 现在在每轮调度中最多检查开始时队列中的
每个任务一次。暂停或依赖未解除的任务会回到 reserved 队列前端，但不会消耗
可用并发槽位；因此并发上限为 1 时，队首被阻塞的任务不会阻塞后续可运行任务。
原有的状态检查、依赖检查和队列顺序保持不变。

Rust-owned 验证：

~~~text
cargo test -p aria2-core --all-features --lib --tests -j 1 --quiet -- --test-threads=1
  exit code 0
  aria2-core library: 3470 passed, 0 failed, 1 ignored
  all aria2-core integration test targets passed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

新增 `blocked_reserved_group_does_not_starve_later_runnable_group` 回归测试，
覆盖 paused 队首任务和后续可运行任务并存时的并发上限语义。本检查点只关闭
reserved 队列的阻塞公平性切片；跨协议生命周期、第三方/原版客户端互操作、
bindings、实测性能和最终 workspace 验收仍未完成，当前阶段保持
`phase-2-core-domain`，整体迁移保持 `PARTIAL`。

## 2026-08-17 HttpResponseProcessor 重试分类检查点

独立 `HttpResponseProcessor` 现在通过 `with_retry_wait` 接收 Rust-owned 的
`retry-wait` 行为配置，不再把重试等待硬编码为 5 秒。来自
`HttpSkipResponseHandler` 的 `RetryableError` 不再被折叠成普通 `Error`，
调用方可以区分可重试和致命的 HTTP 状态；处理器只负责分类，重试计时和
尝试次数仍由下载命令负责。

Rust-owned 验证：

~~~text
cargo test -p aria2-core --all-features --lib http::response_processor -- --test-threads=1
  80 passed, 0 failed
cargo test -p aria2-core --all-features --lib http::skip_response -- --test-threads=1
  36 passed, 0 failed
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3469 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

404 的配置重试、502/503 的 `retry-wait` 门控以及 504 的始终可重试分类均
有回归覆盖。本检查点只关闭独立 HTTP 响应适配器的分类和配置边界；真实
下载命令的更广泛协议生命周期、第三方/原版客户端互操作、bindings、实测
性能和最终 workspace 验收仍未完成，当前阶段保持
`phase-2-core-domain`，整体迁移保持 `PARTIAL`。

## 2026-08-17 BitTorrent in-memory follow 分发检查点

BtTorrentPostDownloadHandler 现在把解析出的 torrent 原始字节保存到生成的
子 RequestGroup。引擎调度器除了识别 bt:// URI 外，也会识别携带
BitTorrent metadata 的子组，因此 tracker/web-seed URI 排在首位时仍会进入
BtDownloadCommand。这修复了 follow-torrent=mem 经过真实 DownloadEngine
时被错误当作普通 HTTP 下载的问题。

Rust-owned 验证：

~~~text
cargo test -p aria2-core --all-features --lib engine::bt_torrent_post_download_handler -- --test-threads=1
  3 passed, 0 failed
cargo test -p aria2-core --all-features --test deep_e2e_bittorrent follow_torrent_mem_http -- --test-threads=1
  2 passed, 0 failed
  engine E2E: parent and child completed, web-seed payload verified,
  source .torrent absent
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

本检查点只关闭 BitTorrent metainfo follow 的子组 metadata 传递和引擎
分发切片；跨协议生命周期、第三方/原版客户端互操作、bindings、实测
性能和最终 workspace 验收仍未完成。当前阶段保持
phase-2-core-domain，整体迁移保持 PARTIAL。

## 2026-08-17 HTTP-date 兼容性检查点

`SimpleDateTime` 现在复用 Rust-owned 的 HTTP-date 解析器；格式化和解析
统一使用有符号的 Gregorian/Unix epoch 换算。统一路径覆盖 IMF-fixdate、
RFC 850 的两位和四位年份、RFC1123 数字时区变体以及 ANSI C asctime，且
正确处理 1970 年前时间和 2038 边界。历史方法名
`parse_rfc2822` 保留以维持现有 Rust API。

Rust-owned 验证：

~~~text
cargo test -p aria2-core --all-features --lib http::conditional_get -- --test-threads=1
  8 passed, 0 failed
cargo test -p aria2-core --all-features --lib http::cookie::tests_date -- --test-threads=1
  9 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

本检查点只关闭 HTTP-date 转换边界；更广泛的条件请求、跨协议生命
周期、第三方/原版客户端互操作、bindings、实测性能和最终 workspace
验收仍未完成，当前阶段保持 `phase-2-core-domain`，整体迁移保持
`PARTIAL`。

## 2026-08-17 RequestGroup 生命周期事件驱动检查点

RequestGroup 现在提供共享的 `tokio::sync::Notify` 生命周期通知。顺序和并行
HTTP、FTP、SFTP，以及内存元数据重试等待，都通过 `tokio::select!` 同时等待
配置的 retry deadline 和生命周期通知；顺序下载的取消等待也使用同一信号。
原来的 50ms 状态轮询已删除。状态和原子控制标志仍是事实来源，通知只负责
唤醒等待者，让等待者重新检查状态。

Rust-owned 验证：

~~~text
cargo test -p aria2-core --all-features --lib engine::sequential_download::tests::retry_wait_wakes_when_removed_after_wait_starts -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --lib request::request_group -- --test-threads=1
  119 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

本检查点只关闭生命周期等待的事件驱动改造。协议调度、性能采样、测试
fixture 和外部 C API 轮询仍属于独立审计项；当前阶段保持
`phase-2-core-domain` (`in_progress`)，整体迁移保持 `PARTIAL`。

## 2026-08-17 HTTP retry-wait 生命周期检查点

顺序 HTTP 下载的重试等待现在通过 RequestGroup 感知的
`SequentialDownloader::wait_for_retry` seam。等待期间会以有界间隔检查
pause、remove 和 halt 状态，因此一次失败的 HTTP 请求不会让任务在完整的
`retry-wait` 配置时间内失去响应。新增的 Rust-owned E2E 使用本地 500 响应
fixture，在真实生产重试等待窗口中分别验证 pause 和 remove；两个任务都能
及时结束，并保留预期的生命周期错误和状态。

Rust-owned 验证：

~~~text
cargo test -p aria2-core --all-features --test test_e2e_download -- --test-threads=1
  40 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

本检查点只关闭真实 HTTP retry-wait 的 pause/remove 生命周期切片；FTP 和
SFTP 的 retry-wait 证据另行记录。更广泛的跨协议生命周期、原版客户端或
第三方互操作、bindings、实测性能证据和最终 workspace 验收仍未完成。当前
阶段保持 `phase-2-core-domain` (`in_progress`)，整体迁移保持 `PARTIAL`。

## 2026-08-17 Concurrent HTTP adaptive-cooldown 生命周期检查点

并行 HTTP range 下载现在通过 RequestGroup 感知的 retry wait 处理 429/503
自适应并发 cooldown。单源分段循环和多镜像 pipeline 都会以有界间隔检查
pause、remove 和 halt，而不是在整个 cooldown 时间内阻塞。Rust-owned E2E
使用本地真实 range server，在一个请求排空的同时返回容量错误，然后在
生成的 cooldown 中分别验证 pause 和 remove。

Rust-owned 验证：

~~~text
cargo test -p aria2-core --all-features --test test_http_adaptive_concurrency_e2e -- --test-threads=1
  7 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

本检查点只关闭并行 HTTP adaptive-cooldown 的 pause/remove 生命周期切片。
当前阶段保持 `phase-2-core-domain` (`in_progress`)，整体迁移保持 `PARTIAL`；
更广泛的跨协议生命周期、原版客户端或第三方互操作、bindings、实测性能
证据和最终 workspace 验收仍未完成。

## 2026-08-17 Session TLS Option Round-Trip Checkpoint

The session option map now preserves configured `certificate` and
`private-key` paths alongside `ca-certificate` and the other HTTP options.
`DownloadOptions::from_option_strings` already consumed these canonical names;
the missing serializer entries meant a restored task silently lost its client
identity paths. The Rust path now matches the relevant aria2_original
`SessionSerializer` boundary, which writes every defined initial task option.
Only the file paths are persisted; certificate and key contents are not copied
into the session file.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib session::session_entry::tests::test_download_options_to_map_all_fields -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_session -- --test-threads=1
  13 passed, 0 failed
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes only the configured TLS client-identity session round-trip slice.
The active phase remains `phase-2-core-domain` (`in_progress`) and the
migration remains `PARTIAL`; broader session lifecycle combinations,
cross-protocol behavior, original-client interoperability, bindings, measured
performance evidence, and final workspace acceptance remain open.

## 2026-08-17 RequestGroup-scoped integrity cancellation checkpoint

Pre-download piece-hash validation now observes the lifecycle of its owning
`RequestGroup`. `CheckIntegrityMan::cancel_gid` removes matching queued work
and notifies its waiter, while an active entry is cooperatively cancelled
between validation chunks. The group-aware enqueue path checks for removal,
pause, and halt every 10 ms, cancels the matching manager entry, waits for
worker cleanup, and returns the lifecycle-specific error. HTTP and BitTorrent
download commands now use this path for their existing-payload integrity
checks.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib checksum::check_integrity::man::tests -- --test-threads=1
  14 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_download test_e2e_http_check_integrity -- --test-threads=1
  3 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download integrity -- --test-threads=1
  4 passed, 0 failed
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3460 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes the RequestGroup-scoped pre-download integrity cancellation
boundary for the covered HTTP and BitTorrent commands. It does not close
broader cross-protocol lifecycle combinations, original-client or third-party
interoperability, bindings, measured performance evidence, or final workspace
acceptance; the active phase remains `phase-2-core-domain` (`in_progress`) and
the migration remains `PARTIAL`.

## 2026-08-17 Current Rust Workspace Regression Checkpoint

The current worktree, including `aria2-core 0.3.2` and the scoped integrity
cancellation changes, passed the all-features Rust workspace regression. This
is a workspace compatibility gate only; ignored tests retain their declared
status and do not count as passing evidence.

Rust-owned verification:

~~~text
cargo test --workspace --all-features -j 1 --quiet -- --test-threads=1
  exit_code=0; all executed targets passed; ignored tests retained their declared status
  aria2-core library target: 3460 passed, 0 failed, 1 ignored
~~~

This does not close Python/Node package validation, platform ABI matrices,
original-client or browser-extension interoperability, measured performance,
or final acceptance. The active phase remains `phase-2-core-domain`
(`in_progress`) and the migration remains `PARTIAL`.

## 2026-08-17 BitTorrent `bt-stop-timeout` checkpoint

The Rust BitTorrent piece loop now consumes the existing `bt-stop-timeout`
option. It reads the live RequestGroup option each cycle, treats both an
unset value and explicit `0` as disabled, and resets the no-progress checkpoint
when completed piece bytes advance. When no peer or web-seed source is
available, the loop remains alive for peer discovery so a configured timeout
can apply. Expiry requests a force halt with `HaltReason::Timeout` and records
`DownloadResultCode::TimeOut`, preserving the public timeout result mapping.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib engine::bt_download_execute::execute::piece_download::tests -- --test-threads=1
  2 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download test_e2e_bt_stop_timeout_returns_timeout_result_without_peers -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download -- --test-threads=1
  32 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes only the BitTorrent no-progress timeout slice. The active phase
remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; broader scheduler and seeding parity, dependency coverage,
original-client and third-party interoperability, bindings, measured
performance evidence, and final workspace acceptance remain open.

## 2026-08-17 BitTorrent `bt-seed-unverified` checkpoint

The Rust BitTorrent path now implements the original
`bt-seed-unverified` behavior for an existing payload. The option defaults to
`false`, is accepted by option-string parsing and runtime `changeOption`, and
is serialized through the session option map. When enabled for an existing
payload, the command marks all torrent pieces complete without validating their
piece hashes and skips the piece writer, so the existing bytes are not
rewritten. Explicit `hash-check-only` retains precedence and still validates
the payload.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib request::request_group::options::tests -- --test-threads=1
  PASS
cargo test -p aria2-core --all-features --lib request::request_group::tests::test_update_option_new_runtime_changeable -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --lib session::session_entry::tests -- --test-threads=1
  PASS
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download -- --test-threads=1
  31 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

The E2E fixture deliberately corrupts an existing payload, enables both
`check-integrity` and `bt-seed-unverified`, and verifies successful completion
with the corrupted bytes unchanged. This closes only the unverified-existing-
payload BitTorrent slice; the active phase remains `phase-2-core-domain`
(`in_progress`) and the migration remains `PARTIAL`. Broader scheduler and
seeding parity, dependency coverage, original-client and third-party
interoperability, bindings, measured performance evidence, and final workspace
acceptance remain open.

## 2026-08-17 Metalink torrent-metadata lifecycle checkpoint

The Metalink BitTorrent metadata request now observes the owning
`RequestGroup` lifecycle before connecting and while reading the response body.
The request and body future are cancelled when the task is paused, removed, or
halted, so a slow metadata server cannot keep the task alive until the full
response or network timeout. Existing HTTP status classification and transport
error behavior are unchanged.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib engine::metalink_download_command::execution::http_status_tests -- --test-threads=1
  4 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_metalink_download -- --test-threads=1
  18 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --test test_e2e_metalink_lifecycle -- --test-threads=1
  13 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download -- --test-threads=1
  30 passed, 0 failed, 2 ignored
cargo test -p aria2-core --all-features --lib -- --test-threads=1
  3453 passed, 0 failed, 1 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
~~~

This closes only the Metalink torrent-metadata lifecycle cancellation slice.
The active phase remains `phase-2-core-domain` (`in_progress`) and the
migration remains `PARTIAL`; third-party and original-client interoperability,
broader cross-protocol lifecycle combinations, bindings, measured performance
evidence, and final workspace acceptance remain open.

## 2026-08-17 Full Rust workspace test checkpoint

The all-features Rust workspace test command completed successfully on the
current worktree. It exercised every workspace crate's unit, integration, E2E,
stress, performance, and doctest target selected by Cargo; ignored tests remain
explicitly ignored and are not counted as passing evidence. The core library
target in this aggregate reported `3453 passed, 0 failed, 1 ignored`.

Rust-owned verification:

~~~text
cargo test --workspace --all-features -j 1 --quiet -- --test-threads=1
  exit_code=0
  all executed targets: passed; ignored tests retained their declared status
~~~

This closes the current Rust workspace aggregate test run only. Platform ABI
matrices, Python/Node package and platform validation, original-client and
browser-extension interoperability, measured aria2 C performance comparison,
and final acceptance remain open; the active phase remains
`phase-2-core-domain` (`in_progress`) and the migration remains `PARTIAL`.

## 2026-08-17 BitTorrent uTP outgoing handshake checkpoint

The uTP socket now routes a `StSyn` received from the address of an existing
outgoing `SynSent` connection back through that connection's state machine. It
therefore treats the packet as the expected SYN-ACK and reaches `Established`;
new addresses retain the inbound-accept path. Before this correction, every
SYN was treated as a new inbound connection and a socket-level outgoing uTP
handshake could never complete. This is Rust-native BEP 29 state routing and
does not change CLI/configuration, tracker, or BitTorrent wire fields.

Rust-owned verification:

~~~text
cargo test -p aria2-protocol --all-features --lib bittorrent::utp::socket::tests -- --test-threads=1
  9 passed, 0 failed
cargo test -p aria2-protocol --all-features --test utp_e2e_test -- --test-threads=1
  53 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download -- --test-threads=1
  30 passed, 0 failed, 2 ignored
cargo clippy -p aria2-protocol --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

This closes only the outgoing uTP socket-handshake routing slice. The active
phase remains `phase-2-core-domain` (`in_progress`) and the migration remains
`PARTIAL`; full BitTorrent scheduler/seeding parity, live original-client and
third-party interoperability, bindings, measured performance evidence, and
final workspace acceptance remain open.

## 2026-08-17 RequestGroup active-slot drain checkpoint

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

## 2026-08-17 HTTP existing-payload integrity recovery checkpoint

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

## 2026-08-17 FTP/SFTP retry-wait cancellation checkpoint

FTP and SFTP retry backoff now observes the owning `RequestGroup` lifecycle
flags in bounded intervals. Paused, removed, and halted tasks leave a retry
wait promptly instead of sleeping for the full configured `retry-wait`
duration. Ordinary retry timing and the existing total-attempt `max-tries`
contract are unchanged; no public option, default, session format, RPC wire
value, protocol wire value, or product identity changed.

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
acceptance remain open. The standalone `HttpResponseProcessor` adapter now has
Rust-owned retry-wait configuration and preserves retryable versus fatal result
classification; no production caller was found, so broader live protocol
interoperability remains a separate open item.

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
  library: 3451 passed, 0 failed, 1 ignored
  all integration, E2E, stress, and performance targets: passed
cargo test -p aria2-core --all-features --benches
  exit_code=0; all benchmark targets completed successfully
~~~

This closes the broad local Rust-owned core regression and lifecycle evidence
slice only. Live third-party services, original-client or browser
interoperability, platform-specific behavior, bindings, and final workspace
acceptance remain open. The active phase remains `phase-2-core-domain`
(`in_progress`) and the migration remains `PARTIAL`.

## 2026-08-17 Removed Status Predicate Checkpoint

`DownloadStatus::is_stopped()` now includes `Removed`, matching the
Rust-owned stopped-result store and RPC status contract. Active and waiting
remain the only non-stopped states.

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

## 2026-08-16 BitTorrent Save-Session Checkpoint

The BitTorrent checkpoint owner now treats an explicit session-save request as
a durable boundary. After a verified peer or web-seed piece is written, the
owner flushes its positioned/cache-backed writer before saving the matching
piece bitfield and consumes the request only after both operations succeed.
The regression uses the real `SaveSessionCommand` and a slow web-seed fixture;
it reads a piece named complete by the sidecar back from disk to verify that
the writer flush happened before checkpoint publication.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download test_e2e_bt_save_session_flushes_requested_checkpoint -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_bittorrent_download -- --test-threads=1
  30 passed, 0 failed, 2 ignored
~~~

This closes the explicit BitTorrent save-session checkpoint and writer-flush
slice only. The migration remains `PARTIAL`; broader lifecycle, protocol
interoperability, bindings, performance, and final workspace acceptance are
still open.

## 2026-08-16 Concurrent HTTP Save-Session Checkpoint

The single-mirror concurrent HTTP owner now shares the explicit control-file
flush helper with the multi-mirror owner. The helper is reached at startup,
write, segment-completion, and cancellation-timer boundaries; it flushes the
writer, updates the committed segment progress, saves the `.aria2` sidecar,
and consumes the request only after the save succeeds. The regression waits
for committed sidecar progress rather than transient in-flight progress before
invoking the real `SaveSessionCommand` path.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --test test_e2e_concurrent_http_range test_concurrent_save_session_flushes_requested_control_file -- --exact --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --all-features --test test_e2e_concurrent_http_range -- --test-threads=1
  9 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes the explicit concurrent HTTP save-session owner slice only. The
migration remains `PARTIAL`; broader lifecycle, protocol interoperability,
bindings, performance, and final workspace acceptance are still open.

## 2026-08-16 Stopped-Result GID Uniqueness Checkpoint

The stopped-result store now enforces the original `IndexedList` contract that
each terminal result is keyed uniquely by GID. A replayed or racing lifecycle
path is rejected without changing the first result or its FIFO position, so
`tellStopped`, `getDownloadResult`, removal, and pruning cannot expose duplicate
terminal entries for one task.

Rust-owned verification:

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

Rust-owned verification:

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

## 2026-08-16 Core HTTP lifecycle checkpoint

The engine-level sequential HTTP regression now covers the complete
pause/unpause and removal lifecycle through `DownloadEngine`: pausing a live
stream saves a non-empty partial control file, unpause allows the group to be
re-promoted and finish, and removal retains both the partial output and its
control file. The pause fixture intentionally ignores Range requests, so the
test sets `always_resume=false` to exercise the explicit fresh-download
fallback after the saved checkpoint is observed. The test therefore does not
claim range-resume interoperability.

Rust-owned verification:

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

This closes the local sequential HTTP engine pause/unpause/removal lifecycle
slice only. Third-party HTTP range behavior, broader cross-protocol lifecycle
coverage, owner-side integrity-plan application, interoperability, and final
workspace acceptance remain open; the migration remains `PARTIAL`.

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

Rust-owned verification:

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
remains `phase-2-core-domain` (`in_progress`), the migration remains `PARTIAL`,
and broader lifecycle, cross-protocol interoperability, bindings, performance,
and final workspace acceptance remain open.

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

Rust-owned verification:

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
acceptance remain open; the migration remains `PARTIAL`.

## 2026-08-16 Integrity Callback Dispatch Plan Checkpoint

The legacy `StreamCheckIntegrity` and `BtCheckIntegrity` wrappers now return
explicit Rust-owned plans for incomplete-check reset/allocation, successful BT
allocation and hook selection, and trailing-garbage cleanup. The plans carry
physical file paths and declared lengths and preserve hash-check-only behavior.
The BT incomplete branch now retains the original allocation behavior.

These methods describe work instead of performing registry lookups or blocking
async I/O: the owning command remains responsible for mutable `PieceStorage`
access and the existing file-allocation/truncation managers. Production
downloads already use the direct `CheckIntegrityTask`/`IntegrityOutcome` seam,
so the legacy wrapper owner-side application is still recorded as `PARTIAL`.

Rust-owned verification:

~~~text
cargo test -p aria2-core --all-features --lib checksum::check_integrity -- --test-threads=1
  56 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo test --workspace --all-targets --all-features --quiet
  PASS
~~~

This closes the callback-plan interface and regression slice only. Owner-side
application evidence, broader protocol lifecycle coverage, interoperability,
and final workspace acceptance remain open.

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

Rust-owned verification:

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

Segmented HTTP Range downloads now classify HTTP 429 as a typed
`ServerError`, so the adaptive concurrency controller recognizes capacity
responses, reduces admission, and requeues the affected ranges without
consuming the ordinary segment retry budget. The single-URI executor no longer
reports success while rate-limited ranges remain unwritten.

The affected segmented HTTP fixtures now preserve `min-split-size` in each task snapshot.
The adaptive fixture uses an 8 MiB payload with a Rust-owned `1M` snapshot, so
the 429, multi-mirror, shared-authority, split-budget, and cancellation cases
exercise valid segmented ranges instead of relying on an implicit default.

Rust-owned verification:

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

The E2E regression verifies exactly one request for each successful initial
range and one retry for each initially rate-limited range, followed by an
exact byte-for-byte output comparison. This closes only the adaptive segmented
HTTP capacity-retry slice; broader protocol lifecycle combinations,
third-party interoperability, original-client interoperability, and final
workspace acceptance remain open. The migration remains `PARTIAL`.

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
the migration remains `PARTIAL`.

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
acceptance remain open; the migration remains `PARTIAL`.

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
interoperability, and final workspace acceptance remain open; the migration
remains `PARTIAL`.

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
streaming 403 responses, plus direct 404/503 classifier coverage. This closes
only the segmented Range classification slice; skip-response `max-file-not-found`,
broader protocol lifecycle combinations, and original-client interoperability
remain open, so the migration remains `PARTIAL`.

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
original-client interoperability remain open, so the migration remains
`PARTIAL`.

## 2026-08-15 Retry Error Classification Checkpoint

`RetryPolicy::should_retry` now has an explicit retryable-error allowlist.
Timeouts, temporary network failures, and configured HTTP `ServerError` codes
remain eligible; HTTP 404, `CannotResume`, authentication failures, redirect
failures, range failures, and other protocol errors are terminal. The public
`max-tries` contract is unchanged: it counts total attempts and `0` remains
unlimited.

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

The Rust-owned HTTP fixture confirms two total attempts for a persistent 500
and one request for a persistent 404. This checkpoint closes only the shared
retry-policy classification seam; broader 4xx mapping, cross-protocol retry
semantics, and original-client interoperability remain open, so the migration
continues as `PARTIAL`.

## 2026-08-15 Gap HTTP Status Classification Checkpoint

The sequential gap downloader now preserves `RangeNotSatisfiable` for HTTP
416, retains `ServerError` for HTTP 5xx responses, and maps other HTTP
failures to `HttpProtocolError` instead of a fatal configuration error. This
aligns the gap result path with ordinary sequential HTTP status classification.

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

Metalink status mapping and the remaining range/4xx paths remain open phase-2
work; the migration remains `PARTIAL`.

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
the migration remains `PARTIAL`.

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

## 2026-08-15 Sequential HTTP Cancellation Checkpoint

Sequential HTTP body reads now race each pending `bytes_stream` item against
the RequestGroup cancellation watcher. The in-memory metadata path uses the
same cancellation tick. Pause and remove therefore interrupt a stalled
response without waiting for the server to deliver another body chunk. The
existing finalize-before-checkpoint ordering is shared by both the
between-chunk and pending-read cancellation paths.

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

The slow-stream regressions verify that a partial sequential HTTP download
remains paused with a Rust-owned `.aria2` checkpoint and that an in-memory
metadata download remains paused without creating an output file. This is
phase-2 lifecycle evidence only; the overall migration remains `PARTIAL`.

## 2026-08-15 RequestGroup Autosave Terminal Filter Checkpoint

`RequestGroupMan::request_control_file_saves` now requests persistence only
for waiting, active, and paused groups. Complete, error, and removed groups
are excluded even during the short interval before their scheduling entry is
demoted, so a session save cannot leave stale autosave requests on terminal
tasks.

Rust-owned verification:

~~~text
cargo test -p aria2-core --lib request::request_group_man --all-features -- --test-threads=1
  32 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes the terminal-state filtering slice of the phase-2 RequestGroup
seam. Full cross-protocol lifecycle, session variants, and interoperability
evidence remain open; the overall migration remains `PARTIAL`.

## 2026-08-15 Gap-download Cancellation Checkpoint

Concurrent HTTP fallback now reaches the Rust sequential gap downloader with
the completed ranges preserved. Both the ranged request and each pending body
chunk race the RequestGroup cancellation watcher; a pause or removal cleans
the partial gap and returns promptly instead of waiting for another network
chunk. The Rust-owned fixture forces a real `416` fallback and stalls the
next ranged body read.

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
cross-protocol E2E, and original-client interoperability keep the overall
migration `PARTIAL`.

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
owner-side integrity callback application, protocol command lifecycle coverage, session
variants, cross-protocol E2E, and original-client interoperability keep the
overall migration `PARTIAL`.

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
cross-protocol E2E, and original-client interoperability remain open, so the
overall migration remains `PARTIAL`.

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
migration remains `PARTIAL`.

## 2026-08-17 Independent Crate Versions And Binary Release Identity

The four Rust workspace members now own explicit package versions. The
`aria2` package is the binary release source for `aria2c --version`, the
startup banner, RPC `aria2.getVersion`, binary identity defaults, release tags,
and binary distribution artifacts. Library package versions remain independent;
their path dependency ranges describe API compatibility and are not release
tags for the binary. The release workflow validates all member versions but
reads the binary release only from `aria2/Cargo.toml`.

The 2026-08-15 `0.3.0` entry below remains historical evidence for the previous
release identity.

## 2026-08-15 Auto-save Checkpoint And 0.3.0 Release Identity

`SaveSessionCommand` now requests a control-file flush for every non-terminal
group before serializing the session. The active protocol command remains the
owner of its in-memory `ControlFile`, `ProgressCheckpoint`, or `BtCheckpoint`;
it consumes the request only at its existing durable write boundary. The seam
covers sequential and concurrent HTTP, FTP/FTP proxy, SFTP, Metalink, and
BitTorrent paths without copying protocol state into `RequestGroupMan`.

Rust-owned verification:

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

The autosave regression proves a request is consumed by an active checkpoint
owner and that a real `.aria2` file contains the updated completed length. The
overall migration remains `PARTIAL`; live interval timing, broader protocol
E2E, original-client interoperability, and the remaining phase-2 gates remain
open. The workspace and SDK product identity is now `aria2-rust 0.3.0`.

## 2026-08-15 Phase 1 Baseline Matrix Gate

The single current matrix and Goal control record now explicitly own all
audited Rust surfaces: the 20 detailed migration records, the 415 C++ source
units plus 115 implementation-only headers (the public C API header is tracked
separately), all four Rust crates and feature sets, tests/fixtures, C/Python/
Node bindings, examples, benchmarks, and CI workflows. The reference tree is
used only for audit evidence; the targeted Rust-source search found no test
runtime that reads, builds, links, starts, or dynamically loads it.

Phase-gate verification on this checkout:

~~~text
cargo fmt --all -- --check
  PASS
git diff --check
  PASS
cargo test --workspace --all-targets --no-run
  PASS
cargo clippy --workspace --all-targets -- -D warnings
  PASS
focused RPC relocation tests
  5 + 2 + 7 passed, 0 failed
~~~

This closes only the baseline/matrix phase. The unique Goal control in
`docs/compatibility-status.md` records `phase-1-baseline-matrix` as
`passed_locked` and activates `phase-2-core-domain`; protocol compatibility,
full lifecycle evidence, original-client interoperability, bindings, measured
performance, and final workspace acceptance remain open.

## 2026-08-15 Test Baseline Boundary

Compatibility regression tests are Rust-owned and self-contained. Checked-in
fixtures and assertions are the only test inputs; the test process does not
read, build, or link against `aria2_original`. The reference implementation
remains audit evidence for public behavior, while `aria2-rust` owns its
implementation, product identity, defaults, and extension aliases. This keeps
external CLI/config/RPC/protocol compatibility separate from internal C++
structure and does not change any user configuration.

> 目标：以 **aria2_original** 为兼容基准，完成完整兼容且高性能的 Rust 现代下载引擎迁移。
> aria2-next 增强项仅选择性采纳（日志轮转、tail reclaim 等小型项），ED2K 协议缓后。
>
> 当前真实完成度以 [docs/compatibility-status.md](compatibility-status.md) 为准。
> 本文件是迁移过程的历史主台账。逐文件对照结果记录在 `docs/migration/<module>.md`，
> 每个 C++ 单元只审计一次并登记日期，**避免重复对比或遗漏对比**。

## 决策记录（用户确认）

| 日期 | 决策 |
|---|---|
| 2026-07-30 | 兼容目标以 aria2_original 为准；aria2-next 仅采纳小型增强，ED2K 缓后 |
| 2026-07-30 | 文档组织：模块级总览（comprehensive_gap_analysis.md）+ 文件级台账（docs/migration/） |
| 2026-07-30 | 执行顺序：先补全 530 单元文件级对照（校正现有文档误差），再按 P0→P1→P2 修复 |
| 2026-07-30 | 性能验证：仅 Rust 侧基准回归（benches/ + 回归测试），不与 C++ 实测对比 |
| 2026-08-09 | 外部兼容优先：RPC/JSON-RPC/XML-RPC/WebSocket、CLI、配置、session、错误码和原版客户端可观察行为必须以 aria2_original 为契约；Rust 架构和性能优化只能发生在该契约之后 |
| 2026-08-09 | 解析 seam 收敛：OptionDef 作为 CLI/config/RPC 的类型校验入口；结构化执行语义（如 BitTorrent `index-out`）复用同一解析结果，不以统一字符串存储替代行为对照 |
| 2026-08-12 | 产品身份统一为 `aria2-rust 0.2.9`；只兼容 aria2_original 的外部 CLI/RPC/协议行为，不复用原版版本报告文本或内部 C++ 结构 |
| 2026-08-15 | 发布身份更新为 `aria2-rust 0.3.0`，同步 Rust workspace、SDK、安装器和发行版元数据；外部 CLI/RPC/协议兼容边界保持不变 |
| 2026-08-15 | CLI/config 选项审计收口：`aria2_original/src/prefs.cc` 的 214 个名称中，Rust registry 覆盖 212 个，`help`/`version` 由 CLI action 处理；全特性 registry 另含 22 个 Rust 扩展。原版短选项与 Rust 新增别名分开记录和测试，保留 `-L/-e/-r/-I/-G/-g/-B/-X`，不修改长选项、配置键、默认值或 RPC wire；四组运行时策略由 Rust 项目自有基线逐项回归，global 允许 3 个 Rust 扩展、reserved 允许 1 个 Rust 扩展 |
| 2026-08-14 | RPC 生命周期语义对齐 `aria2_original`：`forcePause` 在响应前提交 `paused`，reserved 的 `remove`/`forceRemove` 在响应前进入 stopped result，未知 GID 返回 execution error code 1；重复 `pause`/`forcePause` 和非法 `unpause` 状态不再静默成功。active 任务仍由 Rust `EngineCommand` 完成协议取消、checkpoint 与 completion 收口。RPC 测试 19 + 71 + 55、RequestGroupMan 31、engine loop 14 全通过；未修改配置、默认值、版本或 wire 结构，整体迁移仍为 PARTIAL |
| 2026-08-12 | FTP 生产路径补齐协议错误边界、`550` 文件不存在映射和 `REST 0` 行为；本地 FTP E2E 通过，真实第三方 FTP/FTPS 和原版客户端互操作仍为 PARTIAL |
| 2026-08-13 | BitTorrent 共享 TCP listener 与 info-hash 路由完成；MSE PadA/PadB、RC4/Plain negotiation、`bt-force-encryption`、`bt-require-crypto`、`bt-min-crypto-level` 和 listener shutdown 增加真实 socket 回归证据；整体迁移仍为 PARTIAL |
| 2026-08-13 | 修正 `config::runtime` feature-aware 测试断言：启用 BitTorrent 时 Rust-only `enable-public-trackers` 会使 reserved-changeable 集合为 107，默认构建仍为 106；不是兼容行为回退 |
| 2026-08-13 | Rust 并发配置保持产品自有语义：`split=16`、`max-connection-per-server=16`，与内部 HTTP executor/pool 的容量策略一致；不将原版默认值强行覆盖到 Rust 实现 |
| 2026-08-13 | 修复共享 BitTorrent listener 接线后的直接 command E2E 回归：standalone `BtDownloadCommand` 现在拥有独立 Rust listener，engine 路径仍覆盖为进程级共享 listener；显式 `seed-time=0` 优先于默认 `seed-ratio=1.0`，避免下载完成后误进入无限 seeding。BitTorrent E2E `21 passed, 0 failed, 2 ignored`；整体迁移仍为 PARTIAL |
| 2026-08-13 | 对照 `aria2_original` 的 `NO_DEFAULT_VALUE` 语义，修正 `seed-time`：省略时保持未配置，只有显式 `seed-time=0` 才禁用时间条件；`seed-ratio` 仍默认为 `1.0`。新增配置注册、选项映射和 RPC 更新回归测试；整体迁移仍为 PARTIAL |
| 2026-08-13 | HTTP 并发路径补齐多镜像 `.aria2` 生命周期：兼容 bitfield 恢复、stale sidecar 拒绝、周期 checkpoint、pause/remove 与 Range fallback 前取消/排队写入排空/flush/save、成功删除；双镜像真实 HTTP E2E 和 segment-manager 回归通过；跨协议 pause/remove 与 integrity 回调仍为 PARTIAL |
| 2026-08-13 | FTP/SFTP 传输循环补齐 `Removed`、暂停和恢复生命周期：暂停/移除会关闭远端句柄、finalize 本地 writer 并强制保存 Rust `A2CF` checkpoint，unpause 从持久化前缀重新排队；真实慢速 FTP/SFTP E2E 分别为 `29 passed, 2 ignored` 和 `18 passed, 2 ignored`；原版客户端互操作、第三方服务器和全协议生命周期仍为 PARTIAL |
| 2026-08-13 | BitTorrent checkpoint 生命周期补齐：Rust A2CF 绑定 torrent info-hash，校验 piece 位图长度和 trailing bits，要求 payload 存在，按最后一个 piece 的真实长度恢复进度；peer 和 web-seed 完成路径共享保存 seam，halt、pause/resume、verified-piece skip 与边界单测通过；复跑 BitTorrent 命令级 E2E 为 `25 passed, 0 failed, 2 ignored`，整体迁移仍为 PARTIAL |
| 2026-08-13 | Metalink 普通 payload 生命周期补齐：响应体流式写盘，暂停/移除 finalize writer 并强制保存 Rust `A2CF` checkpoint，恢复使用持久化前缀发出 Range，成功完成删除 checkpoint，整文件 hash 与 `<pieces>` 校验改为流式读取；`deep_e2e_cross_protocol` 为 `8 passed`，专用生命周期 E2E 为 `2 passed`，整体 Metalink 与迁移仍为 PARTIAL |
| 2026-08-13 | Metalink torrent graph 增加真实 engine 生命周期证据：`EngineCommand::AddMetalinkGraph` 提交 metadata/payload graph，metadata 只请求一次，`BtDependency` 在 promotion 前安装 Rust torrent context 和 Metalink 输出映射，payload 通过 web-seed 完成；专用 E2E 为 `13 passed, 0 failed, 2 ignored`，整体 Metalink 与迁移仍为 PARTIAL |

| 2026-08-14 | Client TLS transport increment: the existing `check-certificate`, `ca-certificate`, `certificate`, and `private-key` values now flow through one Rust-owned TLS helper in primary HTTP/HTTPS, Metalink HTTP, production BitTorrent HTTP tracker, and BT web-seed clients. Strict PEM parsing, multi-root CA loading, verification-disabled mode, separate PEM identity validation, legacy empty-password PKCS#12 mutual TLS, and PBES2/AES-256-CBC empty-password PFX construction are covered (`15 passed, 0 failed`). No option name, default, user configuration, RPC/CLI wire behavior, or product version changed. AES-128/192-CBC, AES-GCM, alternative PBKDF2 PRFs, plaintext keyBag, unsupported bag types, and the complete original-client HTTPS matrix remain open; the migration remains PARTIAL |
| 2026-08-14 | FTP production path now performs the original `PWD` -> directory-level `CWD` -> file-name `SIZE`/`RETR` sequence through shared Rust path helpers, establishes passive data TCP before `REST`, prepares the active listener before `REST`, and sends `REST 0` for fresh downloads. Production FTP E2E is `32 passed, 0 failed, 2 ignored`; negotiation is `39 passed` and FTP integration is `13 passed`. No option, default, user configuration, product version, or original-client wire contract changed; third-party FTP/FTPS and multi-homed interoperability remain open. |
| 2026-08-14 | FTP `remote-time` now follows the original production order: after `PWD`/`CWD` and before `SIZE`, the Rust command queries optional `MDTM`, parses the RFC 3659 timestamp through the shared FTP parser, and applies it after releasing the writer handle. FTP `dry-run` now stops after `SIZE` metadata discovery without `REST`/`RETR`; the existing `connect-timeout` value now bounds the control connection and greeting. The local FTP E2E is `35 passed, 0 failed, 2 ignored`; unsupported or malformed optional `MDTM` responses remain non-fatal. No option, default, user configuration, product version, or original-client wire contract changed; FTP/FTPS remains `PARTIAL` pending third-party and original-client interoperability. |
| 2026-08-14 | 修复 reserved 下载的暂停竞态：promotion 现在在同一 Rust 写锁内检查状态并只允许 `Waiting` 进入 `Active`；即使 pause flag 已被消费，`Paused` group 也会保留在 reserved queue，必须显式 unpause 后重新 promotion。新增竞态回归，RequestGroupMan `31 passed`，C API 生命周期 `3 passed`，core all-features 并行 lib `3355 passed, 0 failed, 1 ignored`，未修改配置、默认值、产品版本或外部 wire 行为；整体迁移仍为 PARTIAL |

| 2026-08-13 | BitTorrent promotion now validates external `DownloadContext` identity by torrent info-hash; mismatched session or dependency contexts are rebuilt from the current torrent, preventing stale piece hashes, paths, and mirror mappings. Constructor verification is `37 passed, 0 failed`; the migration remains PARTIAL |

| 2026-08-14 | 清理未使用的旧 Rust `aria2-core::option::OptionHandler`：workspace、examples、bindings 和生产代码均无调用，仅有自身 8 个测试；删除重复默认表、自动类型推断和旧配置加载实现，保留 canonical `config::OptionRegistry`/`ConfigParser` 作为唯一配置 seam。当前 core all-features 复跑为 `3355 passed, 0 failed, 1 ignored`，RPC library `228 passed`，CLI all-features tests、相关 Clippy、fmt 和 diff check 通过；未修改用户配置、默认值、RPC/CLI wire 行为或产品版本，整体迁移仍为 PARTIAL |

| 2026-08-14 | RPC XML-RPC 兼容性增量：对照 `aria2_original/src/XmlRpcRequestParserStateImpl.cc` 和 `src/base64.h`，将 `<base64>` 解码收敛到 Rust-owned `rpc_helpers::decode_aria2_base64`，保留原版跳过非字母表字符、宽松输入和 padding 校验语义；新增 parser 回归，RPC XML/HTTP E2E `46 passed`、all-method `55 passed`、server `5 passed`，未修改配置、默认值、产品版本或内部下载架构，RPC 与整体迁移仍为 PARTIAL |

| 2026-08-14 | BitTorrent 完整性校验生命周期增量：对照 `BtCheckIntegrityEntry` 的完成分支，在 Rust-owned command seam 支持 `bt-enable-hook-after-hash-check` 和 `bt-hash-check-seed`；完整 payload 校验成功时按选项触发 BT completion hook，按 seed 选项决定是否进入 tracker/peer 生命周期，并防止完成事件重复发送；新增真实 peer/tracker fixture，BitTorrent E2E `27 passed, 0 failed, 2 ignored`，未修改用户配置、默认值、产品版本或外部 RPC/CLI 名称，BitTorrent 与整体迁移仍为 PARTIAL |

| 2026-08-14 | SFTP 完整性校验生命周期增量：对照 `SftpNegotiationCommand` 与 `ChecksumCheckIntegrityEntry`，Rust SFTP 现在在传输完成前验证 `checksum`，并对已有完整本地文件先校验；匹配时不重复读取远端，失败时回到远端下载路径，只有校验成功才完成 group 并清理 checkpoint；SFTP E2E `12 passed, 0 failed`，未修改配置、默认值、产品版本或外部 wire 行为，SFTP 与整体迁移仍为 PARTIAL |
| 2026-08-14 | BitTorrent 多文件完整性校验增量：新增真实多文件 torrent E2E，验证横跨两个物理文件的 piece 在 `check-integrity` 后只重新请求损坏 piece，并保留最终文件映射；同时移除无 workspace 生产调用方的旧 FTP `file_preparation` 复制层，FTP 运行时仍由 Rust-owned command 直接完成 SIZE、resume、checksum 和 writer 生命周期，未修改配置或外部 wire 行为，整体迁移仍为 PARTIAL |
| 2026-08-14 | 顺序 HTTP 条件 GET 修复：统一复用 Rust 内部精确重定向状态集合 `300/301/302/303/307/308`，排除 `304 Not Modified`，避免无 `Location` 的合法缓存响应被误判为重定向；无条件 `304` 按原版校验为 HTTP protocol error；新增本地 HTTP 下载回归，HTTP 响应/校验定向测试 `88 passed`、真实 `304` 行为回归 `2 passed`，未修改配置、默认值、产品版本或 RPC wire 行为，HTTP 与整体迁移仍为 PARTIAL |
| 2026-08-14 | DNS 候选耗尽刷新：新增 Rust-owned `DnsCache::resolve_with_refresh`，HTTP task spawner 与 FTP control retry 共用“全候选标坏后清除 endpoint 并重新解析”的 seam；新增 localhost 刷新回归，未修改配置、默认值、产品版本或 RPC wire 行为，HTTP/DNS 与整体迁移仍为 PARTIAL |
| 2026-08-14 | 顺序 HTTP 认证重定向修复：认证重试后的 `3xx` 返回有界重定向动作，复用既有重定向计数、URI 跟踪和 cookie 路径；认证工厂在同一任务内保持激活状态，Basic 保护空间按请求目录限定；真实 `401 -> 302 -> 200` 下载回归与 `engine::download_command` `26 passed`、core Clippy 通过，未修改配置、默认值、产品版本或 RPC wire 行为，HTTP 与整体迁移仍为 PARTIAL |
| 2026-08-14 | 修复 DNS 超时归因：RequestGroup 保留 peer 历史但 housekeeping 只标记最近活动 peer，避免并发/镜像任务把所有历史候选批量标坏；RequestGroup `95 passed`、engine loop `14 passed`、Clippy/fmt/diff check 通过，reqwest 连接建立失败仍无法精确暴露选定地址，DNS/HTTP 与整体迁移仍为 PARTIAL |

## 2026-08-14 HTTP Redirect Contract Checkpoint

The Rust protocol response seam now matches `aria2_original/src/HttpResponse.cc`
for redirect classification: `HttpResponse::is_redirect()` returns true only
for `300/301/302/303/307/308` responses that include `Location`, while `304`
remains a conditional response. The standalone Rust redirect helper also now
includes `300 Multiple Choices`. Core skip-response classification uses the
shared status predicate so a recognized `3xx` without `Location` still reaches
the original error path instead of being silently consumed.

This is a Rust-owned protocol correction; it does not copy the C++ command
state machine and does not change option names, defaults, user configuration,
RPC/CLI wire behavior, or the `aria2-rust 0.2.9` product identity.

Verification:

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

#### Multi-file preallocation checkpoint (2026-08-14)

`MultiFileAllocationIterator` and the production `FileAllocationMan` now
share one Rust-owned allocation policy. `prealloc` performs the original
adaptive fallocate probe and uses cooperative zero-fill only when needed;
`falloc`, `trunc`, and `none` remain separate strategies. The `secure-falloc`
setting reaches adaptive/native allocation, and all zero-fill fallbacks start
at the current file length to preserve resumed data. No option, default,
configuration format, crate version, product version, or public RPC/protocol
contract was changed.

The public `preallocate_file(..., "prealloc", ...)` entry point now reaches the
same native allocation path as production allocation. The old Rust-only
`prealloc` truncation shortcut was removed, so the public helper cannot drift
from the download engine's resume-safe behavior.

Focused verification:

~~~text
cargo test -p aria2-core filesystem::file_allocation --lib
  43 passed, 0 failed
cargo test -p aria2-core filesystem::file_allocation_man --lib
  16 passed, 0 failed
cargo test -p aria2-core multi_file_allocation_iterator --lib
  3 passed, 0 failed
cargo fmt --all -- --check
  PASS
~~~

This closes the multi-file allocation parity slice only. Platform-specific
allocation evidence, full workspace tests, original-client interoperability,
and overall migration acceptance remain open.

#### Disk read cache hint checkpoint (2026-08-14)

`DirectDiskAdaptor` and `MultiDiskAdaptor` now share one Rust-owned
best-effort cache-advice helper for the original `readDataDropCache` behavior.
POSIX builds issue `posix_fadvise(DONTNEED)` for the actual bytes read,
including each segment of a cross-file read; non-POSIX builds remain no-op.
The read result and error behavior are unchanged.

Focused verification:

~~~text
cargo test -p aria2-core multi_disk_adaptor --lib --all-features
  44 passed, 0 failed
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

This closes the local disk cache-advice slice only. Platform-specific
filesystem semantics, original-client interoperability, and workspace
acceptance remain open.

The broader HTTP and original-client/browser interoperability matrix remains
`PARTIAL`.

## 2026-08-14 HTTP Transfer-Encoding Contract Checkpoint

The Rust HTTP body-filter seam now validates `Transfer-Encoding` before any
response body is consumed. It accepts only the original aria2-supported
case-insensitive single value `chunked`; `gzip`, `deflate`, `bzip2`, `br`,
`identity`, unknown values, and multi-token values are rejected with an HTTP
protocol error. Transfer decoding runs before the independent
`Content-Encoding` filters, matching the original response pipeline without
copying its C++ command state machine.

The empty-body path is validated as well, so a declared unsupported transfer
encoding cannot bypass protocol checking. No option name, default, user
configuration, RPC/CLI wire behavior, or `aria2-rust 0.2.9` product identity
was changed.

Verification:

~~~text
cargo test -p aria2-core --lib http::stream_filter_tests --all-features -- --test-threads=1
  31 passed, 0 failed
cargo test -p aria2-core --lib http::skip_response --all-features -- --test-threads=1
  36 passed, 0 failed
~~~

The broader HTTP body-stream integration and original-client/browser
interoperability matrix remains `PARTIAL`.

## 2026-08-14 Configuration and Validation Checkpoint

The configuration seam now treats `OptionDef::parse_value` as explicit input
parsing only. Default injection remains an independent
`ConfigParser::apply_defaults` stage through `parse_default_value`. Boolean
values stay exact `true`/`false`, empty text enables only a boolean flag, and
`rpc-secret` rejects an empty explicit value. CLI, config-file, and environment
input reuse the same typed registry path. No user configuration, product
default, or `aria2-rust 0.2.9` version surface was changed.

Focused regressions remain green: 42 option tests, 30 parser tests, 48
config-file tests, 105 CLI tests, 228 RPC library tests, 18 RPC integration
tests, and 55 all-method RPC E2E tests. The RPC concurrent stress target ran
10 tests with 0 failures. All-feature Clippy for core, protocol, RPC, and CLI
with `-D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed
on 2026-08-14. This checkpoint does not close the overall migration; the
external-client, complete protocol, workspace E2E, and benchmark gates remain
open.

## 2026-08-14 Client TLS transport checkpoint

The existing `check-certificate`, `ca-certificate`, `certificate`, and
`private-key` values are now applied through one Rust-owned helper in the
primary HTTP/HTTPS command, every Metalink HTTP client construction path, and
the production BitTorrent HTTP tracker and web-seed clients. The helper
validates the PEM CA bundle strictly, installs every parsed root, accepts both
the explicit PEM certificate/private-key form and the original empty-password
PKCS#12 single-file form, preserves the PFX certificate chain, and returns
configuration errors at client construction. This is an internal Rust
transport seam; it does not copy the C++ TLS context or alter the public
configuration contract. A checked-in Rustls fixture now exercises the same
seam against a live local HTTPS server: custom CA verification, disabled
server-certificate verification, separate PEM mutual TLS, and legacy single-file
PKCS#12 mutual TLS all complete successfully. A checked-in modern fixture also
constructs a reqwest identity from PBES2/AES-256-CBC PFX data. AES-128/192-CBC,
AES-GCM, alternative PBKDF2 PRFs, plaintext keyBag, unsupported bag types, and
the broader original-client HTTPS matrix remain open.

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

The empty-password PKCS#12 single-file client certificate form is implemented
through a pure-Rust adapter. Legacy PFX mutual TLS and modern PBES2/AES-256-CBC
identity construction are covered. AES-128/192-CBC, AES-GCM, alternative
PBKDF2 PRFs, plaintext keyBag, unsupported bag types, and complete
original-client HTTPS interoperability are still required before this area can
move beyond `PARTIAL`.

#### Retry policy internal seam checkpoint (2026-08-14)

The Rust-owned retry policy now has one millisecond-preserving backoff
implementation for both `compute_wait` and the direct `wait_duration` seam.
Custom backoff factors and sub-second test policies no longer pass through a
second-based truncating implementation. The public `max-tries` contract is
unchanged: it counts total attempts and `0` remains unlimited. This is internal
Rust cleanup and does not change option names, defaults, or wire behavior.

Focused verification:

~~~text
cargo test -p aria2-core --lib engine::retry_policy --all-features -- --test-threads=1
  16 passed, 0 failed
cargo test -p aria2-core --test test_retry --all-features -- --test-threads=1
  15 passed, 0 failed
cargo test -p aria2-core --test test_error_network --all-features -- --test-threads=1
  32 passed, 0 failed, 2 ignored
~~~

## 对照范围

- `aria2_original/src`：415 个 `.cc` + 115 个 header-only = **530 个对照单元**
- 台账骨架历史上由 `scripts/gen_migration_matrix.py` 生成；当前 checkout 的 `scripts/` 目录不包含该脚本，不能把“可重新生成”当作当前工具能力

## 2026-08-13 FTP/SFTP lifecycle checkpoint

FTP and SFTP transfer loops now treat pause and removal as explicit lifecycle
outcomes. The command closes the remote handle, finalizes the local writer, and
forces a progress checkpoint before publishing the result. An unpause promotes
a fresh command, which resumes from the persisted prefix; successful completion
removes the checkpoint.

The `.aria2` suffix is retained as a familiar external path, but the checkpoint
payload is the Rust-owned `A2CF` format. It is not claimed to be binary-
compatible with an aria2_original sidecar. This keeps the public compatibility
contract at the CLI/config/RPC/protocol seam while leaving persistence
implementation autonomous.

Verification:

~~~text
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  29 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test test_e2e_sftp_download --all-features -- --test-threads=1
  18 passed, 0 failed, 2 ignored
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
~~~

These results are focused local-server evidence. They do not close the full
original-client matrix, third-party FTP/SFTP interoperability, public-key SFTP
authentication, checksum-integrity callback parity, or workspace acceptance.

## FTP Proxy Production Checkpoint (2026-08-14)

FTP proxy options are now wired into the Rust-owned production FTP command
without adding a compatibility layer around the C++ implementation. The
existing option names and defaults remain unchanged:

- `proxy-method=get` sends an absolute `ftp://` request target to an HTTP
  forward proxy and consumes the response with the shared Rust HTTP header
  parser.
- `proxy-method=tunnel` uses HTTP CONNECT and then performs normal Rust FTP or
  FTPS negotiation through the tunnel.
- `ftp-proxy-*` credentials take precedence over `all-proxy-*`; `no-proxy`
  bypasses the proxy at the target-host seam.
- GET responses use the existing Rust writer, resume offset, checkpoint,
  checksum, rate-limit, cancellation, and in-memory download paths.

The implementation does not alter user configuration, registry defaults,
CLI/RPC spellings, the `aria2-rust 0.2.9` version, or public protocol values.
It also does not claim `.aria2` persistence or internal state compatibility
with `aria2_original`; only the external compatibility seams are shared.

Verification:

~~~text
cargo test -p aria2-core --test test_e2e_ftp_proxy --all-features -- --test-threads=1
  12 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  35 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib ftp::connection::negotiation --all-features -- --test-threads=1
  39 passed, 0 failed
~~~

This checkpoint remains focused local-fixture evidence. Third-party proxy
authentication and routing, redirects, chunked proxy responses, FTPS through
forward proxies, and the complete original-client/browser matrix still need
independent interoperability tests before FTP or the overall migration can
move beyond `PARTIAL`.

## 2026-08-14 FTP remote-time, dry-run, and connect-timeout checkpoint

The production FTP command now keeps the original `remote-time` contract while
remaining Rust-native internally. After the existing `PWD`/directory-level
`CWD` traversal, it sends `MDTM <file>` before `SIZE`; a valid
`YYYYMMDDhhmmss` response is parsed by the shared FTP timestamp seam. Once the
Rust disk writer has flushed and released its handle, the command applies the
timestamp to the completed local file. Unsupported, malformed, or unavailable
optional `MDTM` responses do not turn a valid download into a failure. When
`dry-run=true`, the command stops after `SIZE`, marks the discovered length as
complete, and does not create an output file or open a data transfer.
The existing `connect-timeout` option is used for the control connection and
greeting instead of a Rust-only fixed timeout; its original default remains
the registry's 60 seconds.

Verification:

~~~text
cargo test -p aria2-core --lib ftp::connection::negotiation --all-features -- --test-threads=1
  39 passed, 0 failed
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  35 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test ftp_integration_test --all-features -- --test-threads=1
  13 passed, 0 failed
cargo fmt --all -- --check
  PASS
~~~

This is a focused FTP behavior checkpoint, not a claim that FTP/FTPS or the
overall migration is complete. Third-party server, multi-homed process, and
original-client interoperability evidence remains open.

## 2026-08-13 BitTorrent MSE/listener checkpoint

The process listener is now a shared Rust-owned socket with per-torrent
info-hash routes. Route handles unregister on drop and shutdown waits for the
listener task to stop before the port is reused. Incoming legacy handshakes
are policy-checked after their info-hash is parsed; MSE handshakes conceal the
info-hash until `req2 ^ req3` verification succeeds.

The MSE implementation keeps the aria2-compatible wire sequence while using
Rust state ownership internally. The VC marker uses a look-ahead RC4 state,
the main decryptor consumes the VC exactly once, and post-handshake RC4 state
continues from the negotiation stream. The initiator also waits for the full
PadD response boundary before reading the following BitTorrent handshake;
this fixed the intermittent plaintext-after-MSE boundary failure.

Verification on 2026-08-13:

~~~text
cargo test -p aria2-protocol --features bittorrent mse_handshake --lib -- --test-threads=1  18 passed
cargo test -p aria2-protocol --features bittorrent incoming::tests --lib -- --test-threads=1  3 passed
cargo test -p aria2-core --features bittorrent bt_peer_listener::tests --lib -- --test-threads=1  3 passed
cargo check -p aria2-protocol --features bittorrent  PASS
cargo check -p aria2-core --features bittorrent      PASS
~~~

These are focused compatibility results. Full BitTorrent scheduler,
dependency, seeding, original-client interoperability, workspace E2E, and
aria2 C performance comparison remain acceptance work.

## 2026-08-13 BitTorrent checkpoint lifecycle

BitTorrent keeps the familiar `.aria2` sidecar path but uses the Rust-owned
`A2CF` format. A checkpoint is accepted only when it is marked as torrent state,
matches the current total length and info-hash, has a valid piece bitfield, and
has a payload on disk. Restore computes completed bytes from each piece's actual
length, including a shorter final piece. Invalid trailing bits, a different
torrent identity, a missing payload, or malformed state are discarded. Peer and
web-seed piece completion both update the same in-memory and durable snapshot;
successful completion removes the sidecar.

Focused verification:

~~~text
cargo test -p aria2-core --lib engine::bt_checkpoint --all-features -- --test-threads=1
  4 passed, 0 failed
cargo test -p aria2-core --test test_e2e_bittorrent_download --all-features -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo check -p aria2-core --all-features
  PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

## RPC Multicall Envelope Checkpoint (2026-08-14)

The `system.multicall` adapter now preserves the original authorization seam:
the envelope does not consume a `token:` parameter, and parameter zero must
be the list of sub-call structs. Each protected sub-call authenticates and
strips its own leading `token:` value. A missing or mistyped envelope
parameter returns execution error `code=1`; a sub-call with a missing, null,
object, or scalar `params` member receives an empty positional list.

This is implemented in the Rust wire/dispatch adapter without copying the C++
method hierarchy or changing CLI, configuration, default, or product-version
values. The overall migration remains `PARTIAL`; this checkpoint covers only
the multicall envelope and sub-call parameter seam.

This checkpoint closes the Rust-owned BitTorrent persistence slice only. It does
not establish binary `.aria2` interoperability with aria2_original, complete
BitTorrent scheduler/seeding parity, Metalink lifecycle parity, the complete
original-client matrix, workspace acceptance, or the aria2 C++ performance
comparison.

## 2026-08-13 BitTorrent web-seed and integrity checkpoint

The command-level BitTorrent fixture now exercises two distinct public
behaviours without importing the original implementation: a torrent with no
peer is served through its `url-list` web seed, and a paused web-seed transfer
resumes from the Rust checkpoint before removing that checkpoint on success.
The `--check-integrity` path also validates an existing payload against piece
hashes, skips the verified piece, requests only the failed piece, and leaves
the final bitfield and checkpoint lifecycle coherent. `FileChunkValidator`
now preserves verified and failed piece indices through the
`CheckIntegrityTask` interface so the picker can consume the result instead of
starting over with every piece.

Focused verification:

~~~text
cargo test -p aria2-core --test test_e2e_bittorrent_download --all-features -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib checksum::check_integrity --all-features -- --test-threads=1
  9 passed, 0 failed
~~~

This is Rust-native execution behind the compatible torrent/web-seed and CLI
seams. It does not claim binary `.aria2` interoperability, complete original
client interoperability, full scheduler/seeding parity, or workspace
acceptance.

## 2026-08-13 Metalink payload lifecycle checkpoint

The ordinary Metalink payload path now owns a Rust-native streaming lifecycle.
HTTP response bodies are written incrementally through the local disk-writer
seam instead of being collected as one in-memory payload. Pause and removal
finalize the writer and force-save the Rust `A2CF` checkpoint; a later command
restores the persisted prefix and sends a byte `Range` request. Successful
completion removes the checkpoint. Whole-file hashes and `<pieces>` hashes are
verified by streaming the completed file from disk, and invalid validation
attempts discard both the output and checkpoint.

Focused verification on 2026-08-13:

~~~text
cargo test -p aria2-core --test deep_e2e_cross_protocol --features metalink --no-default-features
  8 passed, 0 failed
cargo test -p aria2-core --test test_e2e_metalink_lifecycle --features metalink --no-default-features
  2 passed, 0 failed
cargo clippy -p aria2-core --all-targets --features metalink --no-default-features -- -D warnings
  PASS
cargo check -p aria2-core --features metalink --no-default-features
  PASS
cargo check -p aria2-core --all-features
  PASS
~~~

This closes the ordinary Metalink payload lifecycle slice only. Metalink
metadata dependency/session restoration, torrent fallback interoperability,
complete cross-protocol lifecycle parity, original-client interoperability,
workspace acceptance, and the aria2 C++ performance comparison remain open.

## 2026-08-13 Metalink torrent graph engine checkpoint

The Metalink torrent path now has a process-level regression through the public
engine command seam. The test builds a memory-backed metadata/payload graph,
submits it with `EngineCommand::AddMetalinkGraph`, downloads the torrent
metadata once, resolves `BtDependency`, and waits for the payload group to be
promoted by `DownloadEngine`. The resolved torrent context preserves the
Metalink-selected output path before the BitTorrent command starts; the final
payload is served by a local web seed and is verified byte-for-byte.

The promotion seam now treats the torrent info-hash as the identity boundary
for an externally supplied `DownloadContext`. A matching dependency context is
retained so Metalink-selected paths and mirrors survive promotion; a missing or
mismatched context is rebuilt from the current torrent bytes. This is a
Rust-owned lifecycle invariant and does not copy the original C++ ownership
hierarchy or alter CLI/configuration values.

Focused verification:

~~~text
cargo test -p aria2-core --test test_e2e_metalink_lifecycle --all-features -- --test-threads=1
  13 passed, 0 failed, 2 ignored
cargo test -p aria2-core --test test_e2e_metalink_lifecycle --features metalink --no-default-features -- --test-threads=1
  2 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_download_command_tests --all-features -- --test-threads=1
  37 passed, 0 failed
cargo test -p aria2-core --lib request::request_group::dependency --all-features -- --test-threads=1
  8 passed, 0 failed
cargo test -p aria2-core --lib engine::metalink_request_graph --all-features -- --test-threads=1
  3 passed, 0 failed
cargo test -p aria2-core --test test_e2e_session --all-features -- --test-threads=1
  13 passed, 0 failed
~~~

This closes the tested engine submission, dependency resolution, mapping, and
payload promotion slice. It does not close session graph restoration,
multi-file torrent interoperability across every path, complete original
client interoperability, workspace acceptance, or the aria2 C++ performance
comparison. Metalink and BitTorrent therefore remain `PARTIAL`.

## 2026-08-13 Product identity, defaults, and feature wiring checkpoint

`aria2-rust` owns the product identity and release version. The workspace,
Rust crates, SDK metadata, CLI version action, RPC `aria2.getVersion`, HTTP
User-Agent, and BitTorrent peer identity all resolve to `aria2-rust 0.2.9`.
The upstream C++ version-report text remains intentionally removed. JSON-RPC
`2.0`, Metalink `3.0`/`4.0`, and SFTP `3` remain wire-format versions and are
not product versions.

The public `aria2-rust` option defaults intentionally remain Rust-owned:
`split=16` and `max-connection-per-server=16`, with `split` capped at 128 in
this implementation. These values are not claims about `aria2_original`;
compatibility requires the option names, parsing, RPC shapes, and observable
wire behavior, while internal Rust concurrency policy remains independent.

The Rust-only public-tracker controls remain available as additive input
options, but are deliberately excluded from the original `getGlobalOption` and
task `getOption` projections. This keeps the extension useful to the Rust
engine without adding fields to standard aria2-compatible responses.

`CommandDependencies` now owns the shared services used by task construction.
Metalink commands retain the process BitTorrent registry and listener, and
both torrent fallback paths forward them to `BtDownloadCommand`. This keeps
the Rust ownership model independent of the original C++ class hierarchy while
preserving the incoming peer routing behavior.

Verification on 2026-08-13:

~~~text
cargo test -p aria2 --test test_cli_options --all-features -- --test-threads=1  105 passed
cargo test -p aria2-core --features 'bittorrent,metalink' --lib request::request_group::tests  PASS
cargo test -p aria2-core --features 'bittorrent,metalink' --lib config::parser  27 passed
cargo test -p aria2-core --features 'bittorrent,metalink' --lib metalink_download_command  16 passed
cargo test -p aria2-rpc --all-features --lib handlers::handler_tests::test_get_version_uses_product_version  PASS
cargo check -p aria2 --all-features  PASS
cargo clippy -p aria2-core --features 'bittorrent,metalink' --all-targets -- -D warnings  PASS
cargo fmt --all -- --check  PASS
~~~

These results close the observed default-value and all-feature compilation
regressions only. Complete original-client interoperability, workspace E2E,
and the aria2 C performance comparison remain open.

## 2026-08-13 HTTP concurrent control-file checkpoint

The single- and multi-mirror HTTP concurrent paths now use the same durable
control-file lifecycle seam. `ResumeHelper` remains the authority for whether
an existing `.aria2` sidecar is trusted. A compatible bitfield restores only
fully completed segments; a mismatched layout or untrusted sidecar is not
silently reused. Cancellation and Range fallback cancel admitted HTTP tasks,
drain queued writes, flush the output, and save the completed segment count.
Successful completion removes the sidecar.

Focused verification:

~~~text
cargo test -p aria2-core --test test_e2e_concurrent_http_range test_multi_mirror_resume_restores_completed_segments --all-features -- --test-threads=1  1 passed
cargo test -p aria2-core --test test_e2e_concurrent_http_range test_multi_mirror_without_continue_discards_stale_control_file --all-features -- --test-threads=1  1 passed
cargo test -p aria2-core --lib concurrent_segment_manager --all-features -- --test-threads=1  22 passed
cargo check -p aria2-core --all-features  PASS
cargo fmt --all -- --check  PASS
git diff --check  PASS
~~~

This closes the observed multi-mirror HTTP control-file gap only. Engine-level
pause/remove orchestration across FTP, SFTP, BitTorrent, and Metalink,
integrity-entry callbacks, complete original-client interoperability, and the
aria2 C performance baseline remain open acceptance work.

## 2026-08-09 当前增量检查点

本轮已闭合 BitTorrent `index-out` 的实际执行链：共享的
`parse_index_out` 保留原版累积 `INDEX=PATH` wire 顺序，构造阶段将 1-based
映射同时应用到 `DownloadContext`、`MultiFileLayout` 和单文件
`output_path`。TCP `listen-port` 与 DHT `dht-listen-port` 的端口区间按原版
顺序尝试，首端口被占用时回退到后续端口，并由真实 socket 回归测试覆盖。

本轮还修复了生产 FTP 被动数据连接的兼容差异：`aria2_original` 在
`FtpNegotiationCommand::preparePasvConnect()` 中使用控制连接的 peer 地址，
不使用 PASV 响应中的广告 host；Rust engine 现在保持同一语义。新增本地
E2E fixture 广告错误地址仍能下载的回归，避免把 NAT/错误广告地址误当作
真实数据连接目标。

本轮进一步统一了原版 `--max-tries` 的外部语义：
`aria2_original/src/OptionHandlerFactory.cc` 的默认值为 5，
`AbstractCommand.cc` 将它解释为总尝试次数，`0` 表示无限尝试。Rust 的
`RetryPolicy` 现在作为顺序 HTTP、并发 segment 和 FTP 的共享 seam，统一
使用默认 5 次、`max-tries=0` 无限、退避等待和可重试错误分类；内部保留的
`max_retries` 字段名不改变外部 wire/config 名称或行为。

面向浏览器客户端的 RPC HTTP seam 也修复了一个可观察差异：
`aria2_original/src/HttpServerBodyCommand.cc` 对 OPTIONS preflight 返回
`Access-Control-Max-Age: 1728000`，Rust 现在通过共享 `CORS_MAX_AGE` 常量
返回相同值，并由真实 HTTP E2E 覆盖。这个 header 修复改善原版插件的预检
兼容性，但不等于完整 Chrome/Firefox 插件互操作已经完成。

本轮验证通过：

- `cargo fmt --all -- --check`
- `cargo clippy -p aria2-core --all-targets --all-features -- -D warnings`
- `cargo clippy -p aria2-protocol --all-targets --all-features -- -D warnings`
- `cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings`
- core option tests：34 passed / 0 failed
- BT command tests：34 passed / 0 failed
- TCP listener tests：4 passed / 0 failed
- DHT engine tests：6 passed / 0 failed
- retry executor tests：15 passed / 0 failed
- HTTP `max-tries` E2E：1 passed / 0 failed
- FTP E2E：25 passed / 0 failed / 2 ignored
- FTP-related core unit tests：200 passed / 0 failed / 1 ignored
- RPC HTTP E2E：43 passed / 0 failed

本轮 CLI/config 生命周期对照还确认 `OptionHandlerFactory.cc` 的四组原版
集合没有缺项：`setInitialOption` 113/113、`setChangeGlobalOption` 120/120、
`setChangeOptionForReserved` 106/106、`setChangeOption` 7/7。Rust-only
公共 tracker 控制项作为扩展输入保留，并明确计为 global 的 3 个扩展
（`enable-public-trackers`、`bt-tracker-source`、
`bt-tracker-update-interval`）和 reserved 的 1 个扩展
（`enable-public-trackers`），不伪装成原版注册项。测试
`cargo test -p aria2-core --all-features --test runtime_policy_baseline -- --test-threads=1`
只读取 Rust 项目自有的 `aria2-core/tests/fixtures/compatibility_option_policies.txt`
基线并报告集合差异；本轮结果为 `1 passed, 0 failed`。

这些是增量证据，不代表 530 个 C++ 对照单元都已达到行为兼容，也不代表
workspace、原版浏览器插件和完整端到端矩阵已经全部通过。

## 模块对照进度

**530 / 530 个 C++ 单元已完成逐项源码对照（2026-07-30）。**
这只表示审计记录覆盖，不表示 Rust 已达到行为兼容、协议互操作或
workspace all pass；当前状态请以 docs/compatibility-status.md 为准。
下表按当前 `docs/migration/*.md` 的状态行重算；HTTP 矩阵中的
`RequestGroup lifecycle` 是一个跨模块补充记录，不计入 530 个 C++ 单元。

| 模块 | 矩阵文件 | 单元数 | 完整 | 部分 | 缺失 | 不适用 |
|---|---|---|---|---|---|---|
| auth | migration/auth.md | 7 | 4 | 0 | 0 | 3 |
| bt_core | migration/bt_core.md | 115 | 61 | 5 | 0 | 49 |
| checksum | migration/checksum.md | 7 | 3 | 3 | 0 | 1 |
| command_engine | migration/command_engine.md | 29 | 7 | 7 | 0 | 15 |
| cookie | migration/cookie.md | 5 | 5 | 0 | 0 | 0 |
| dht | migration/dht.md | 61 | 36 | 4 | 0 | 21 |
| event_socket | migration/event_socket.md | 15 | 0 | 1 | 0 | 14 |
| ftp | migration/ftp.md | 9 | 2 | 5 | 0 | 2 |
| http | migration/http.md | 24 | 9 | 5 | 0 | 10 |
| integrity_alloc | migration/integrity_alloc.md | 5 | 3 | 1 | 0 | 1 |
| io_disk | migration/io_disk.md | 26 | 14 | 2 | 0 | 10 |
| lpd | migration/lpd.md | 5 | 5 | 0 | 0 | 0 |
| metalink | migration/metalink.md | 14 | 8 | 3 | 0 | 3 |
| option | migration/option.md | 13 | 7 | 6 | 0 | 0 |
| rpc | migration/rpc.md | 18 | 2 | 11 | 0 | 5 |
| segment | migration/segment.md | 3 | 3 | 0 | 0 | 0 |
| session_app | migration/session_app.md | 33 | 18 | 5 | 0 | 10 |
| sftp | migration/sftp.md | 6 | 0 | 3 | 0 | 3 |
| tls_crypto | migration/tls_crypto.md | 26 | 6 | 0 | 0 | 20 |
| util | migration/util.md | 109 | 38 | 10 | 0 | 61 |
| **合计** | | **530** | **231** | **71** | **0** | **228** |

> "不适用" 占比高（228/530，43%）属预期：C++ 侧大量单元是抽象基类、工厂类、
> SharedHandle 包装、epoll/kqueue 事件循环封装与平台分支实现，
> 在 Rust 的 trait + 所有权 + tokio 异步模型下被语言机制直接取代。
> 每一行均在结论列注明"由什么机制替代"，不存在未说明的跳过项。

### 台账中的 `缺失` 列

当前逐文件矩阵将缺失列记为 **0 项**。这不是行为缺口为零的证明；
例如 C ABI、完整完整性生命周期和部分 E2E 仍在当前状态矩阵中单独跟踪。
2026-07-31 的历史记录曾处理最后 2 项（Sqlite3CookieParser / Sqlite3CookieParserImpl）——
引入 `rusqlite`（`bundled` feature，静态编译 SQLite 进二进制，无系统依赖），
新增 `aria2-core/src/http/sqlite_cookie_parser.rs`：`parse_firefox`（moz_cookies 表）、
`parse_chromium`（Cookies 表，1601 微秒纪元 → UNIX 秒换算）、`parse_auto` +
`is_sqlite_file()` magic 探测；`CookieStorage::load_file` 现按文件内容自动路由
（SQLite magic → rusqlite 解析，否则走 Netscape），对齐 C++ `CookieStorage::load` 行为。
14 个专项测试全绿，全量 4154 测试 0 失败。

> **2026-07-30 第二轮消项**：原 5 项中的 3 项已处理完毕 ——
> `TimedHaltCommand` / `WatchProcessCommand` 已实现并接线（`aria2-core/src/engine/halt_watchers.rs`
> + CLI `spawn_halt_watchers` + RPC shutdown 宽限），状态改为 `完整`；
> `HaveEraseCommand` 经代码复核判定为**架构差异**——Rust 的 have 广播是集中式直推
> （`broadcast_have` 遍历活动连接，O(N)），新 peer 靠握手后的 Bitfield 覆盖，
> 不存在 C++ 那种需要定时清扫的共享公告板；残留的 `haves` 队列已在唯一插入点
> `advertise_piece` 内联 5s TTL 驱逐，无需第二个调度任务。

> **2026-07-30 复核更正**：checksum 模块原登记的 2 项 `缺失`
> （`ChecksumCheckIntegrityEntry` / `IteratableChecksumValidator`）判定有误，已改为 `部分`。
> `--checksum` 在主下载路径**确已生效**——`DownloadCommand::execute` 成功分支以 64 KiB
> 流式复算整文件并在不符时返回 `Aria2Error::Checksum`，HTTP/FTP 均覆盖。
> 真实差距是架构层面：C++ 把校验建模为可被事件循环逐块驱动、可中断的
> `CheckIntegrityEntry`，Rust 目前是同步内联循环，校验大文件期间不上报进度也无法暂停。
> 这类"功能在但架构未对齐"的项统一归入 `部分` 而非 `缺失`。

## 状态取值约定

- `完整`：Rust 实现覆盖 C++ 单元全部行为（或有等价/更优实现），有测试
- `部分`：核心行为已覆盖，存在已登记的差距项（在结论列引用差距编号）
- `缺失`：无对应实现
- `不适用(架构差异)`：C++ 单元在 Rust 架构下无意义（如 epoll 封装、SharedHandle），需在结论列说明由什么机制替代

## 差距清单

校正后的 P0/P1/P2 差距清单以 `docs/comprehensive_gap_analysis.md` 为准，文件级审计发现的新差距项在汇总阶段合并进去。

## 会话日志

### 2026-07-30
- 确认迁移决策（见上表）
- 生成 530 单元 × 20 模块对照台账骨架（历史使用 `scripts/gen_migration_matrix.py`；该脚本当前不在 checkout 中）
- 启动分模块并行逐文件审计
- **530 / 530 单元逐项对照完成**，结果汇总入上方进度表；复核更正后为 5 项 `缺失`、63 项 `部分`
- 阶段 2 消项后降至 **2 项 `缺失`**（均为 cookie 模块的 SQLite 解析器）、63 项 `部分`

### 2026-07-31

- **消灭最后 2 项 `缺失`（SQLite cookie 导入）**，530 单元对照**全模块零缺失**：
  - 决策：用户选定 **rusqlite（bundled）**——静态编译 SQLite，无系统依赖，对齐 C++ `sqlite3_open` 行为；`aria2-core/Cargo.toml` 加 `rusqlite = { version = "0.40.1", default-features = false, features = ["bundled"] }`
  - 新增 `aria2-core/src/http/sqlite_cookie_parser.rs`（`Sqlite3CookieParser`）：
    - `parse_firefox`：moz_cookies 表（host/path/isSecure/expiry/name/value/lastAccessed）
    - `parse_chromium`：Cookies 表（host_key/path/is_secure/expires_utc/name/value/creation_utc），1601 微秒纪元 → UNIX 秒（`- 11_644_473_600`）
    - `parse_auto` + `is_sqlite_file()`：16 字节 `SQLite format 3\0` magic 探测，失败回退 Chromium schema
    - 对齐 C++ `sqlite3_exec` char* 语义：宽松文本时间戳（parseLLIntNoThrow）、host_only = 无前导点、numeric host 恒 host_only；**补了 C++ 缺失的 null 列保护**（Netscape 文本无 null 问题，但 SQLite 列可 NULL）
  - `CookieStorage::load_file` 按文件内容自动路由（SQLite magic → rusqlite；否则 Netscape），对齐 C++ `CookieStorage::load` 的前缀判断
  - **14 个专项测试全绿**（含端到端 `load_file` 探测、Firefox/Chromium 建库、错误 schema 报错、null/空值、文本时间戳）
- 全量验证：`cargo test --workspace --features aria2-core/bittorrent --lib` → **4154 passed / 0 failed / 1 ignored，全程 33 秒无卡死**（较 2026-07-30 的 4140 +14）

#### P0 收官：FileAllocationMan 队列 + 分配 worker（`缺失` 清零后继续清 P0）

- **现状问题**：`file_allocation_man.rs` 只有数据结构骨架（队列 + 11 个单测），三个 `EngineCommand::FileAllocation*` 变体**无任何发送方**（死代码）；HTTP 下载同步整文件 `preallocate_file`（无队列/并发控制/分块），**BT 路径完全不预分配**（`--file-allocation` 对 BT 失效）。
- **实现**（`filesystem/file_allocation_man.rs` 重写，16 测试全绿）：
  - `FileAllocationEntry` 承载 `AllocationKind::{Path, Multi}` + strategy + secure_falloc + `oneshot` 完成通知；picked 改存轻量 `PickedMeta`（含共享取消标志）
  - 后台 `worker_loop`（tokio 任务，等价 C++ FileAllocationDispatcherCommand + FileAllocationCommand）：FIFO 消费、`max_concurrent` 默认 1（对齐 C++ 串行）、每块 `yield_now` 让出（等价 C++ 每 tick allocateChunk）、`cancel_all` 引擎停机清理
  - `Falloc`/`Trunc` 原子系统调用不分块；仅 `Prealloc` 分块 zero-fill（256 KiB，对齐 SingleFileAllocationIterator）；`secure` 标志真正传入 fallocate 路径（旧迭代器硬编码 false）
- **接线**：`shared()` 进程级单例 + 惰性 worker；`lifecycle.rs`/`engine_loop.rs` 的 `file_alloc_man` 统一指向 `shared()`；`on_end_of_run` 调 `cancel_all`；删除 3 个死命令变体与 handler；HTTP `DownloadCommand` 与 BT `BtDownloadCommand`/magnet 均经 `enqueue_path`/`enqueue_multi` 入队；magnet 改用真实 `options()`（原传 `DownloadOptions::default()`）
- **环境事件**：编译中 D 盘 100% 满（653G 用尽，仅剩 61M）→ 删除 `target/debug/incremental`（15G 增量缓存）恢复 12G 可用；`target/debug/.cargo-build-lock` 偶发 os error 5 重试即过
- 全量验证：**4161 passed / 0 failed / 1 ignored**（较上轮 +7），无卡死

#### P1 重审 + Metalink 逐块校验（P1 #5 修复）

- **P1 全表重审**（agent 并行审计 19 项关键差距）：文档严重过时——28 项中 14 项已实现（BtSetup、Cookie DomainNode/LRU、DHT 接入、FTP active mode、WebSocket 通知格式、AuthConfigFactory、reduce_to_limit、control file 清理、option 232 注册、Metalink 过滤/verification 等）、4 项部分（BT 消息工厂上下文注入、WrDiskCache BT 路径、seed 期 re-announce、BT advertise_piece）、4 项仍缺失（#7 metaurl、#11 initStorage、#14 CheckIntegrityMan 队列、#22 Content-Disposition 边角）。P1 表已按真实状态重写。
- **Metalink `<pieces>` 解析 bug 修复**（aria2-protocol）：原 `split_whitespace` 对 v4 `<hash>` 子元素（C++ 标准形式）完全误解析、对 v3 连续文本只产生一个拼接条目；`piece_count()` 错误地除以 hex 长度。修复：`pending_pieces` 状态跟踪，v4 `<hash>` 子元素逐个收集、v3 文本按 `hash_len()` 切块（对齐 C++ `isValidHash` 语义），`piece_count()` 返回 `hashes.len()`。3 个 parser 测试。
- **`verify_pieces()` 实现**（aria2-core）：下载完成后按 `<pieces>` 长度逐块校验（md5/sha1/sha256/sha512），块数/摘要长度不匹配即失败；`FileDownloadInfo` 增 `pieces` 字段，单文件与多文件模式均接线；抽出公共 `digest_hex()`。1 个测试（成功/篡改首尾块/块数不匹配/错误长度/空列表 5 种情况）。
- 全量验证：**4161 passed / 0 failed / 1 ignored**，无卡死。

#### CheckIntegrityMan 队列（P1 #14 修复，历史实现记录）

- **初始现状**：`CheckIntegrityKind`/`StreamCheckIntegrity`/`BtCheckIntegrity` 校验器实现完整但**零调用方**——HTTP/BT 下载路径直接写文件，从不构建 `PieceStorage`，校验器依赖的抽象不存在（孤儿代码）。这一段记录的是修复前审计结论，不是当前状态。
- **实现**（`checksum/check_integrity/man.rs` 新增，5 测试全绿）：
  - `CheckIntegrityMan`：队列 + 后台 worker（C++ CheckIntegrityMan + Dispatcher + Command 语义），默认串行、分块 `validate_chunk` + `yield_now`、`oneshot` 结果通知、`cancel_all`
  - `CheckIntegrityTask` trait（`Send + Sync`）；`FileChunkValidator`：文件直读分块哈希（不依赖 PieceStorage），短读 → mismatch 而非 I/O 错误（对齐 C++ 不完整文件判失败重下）
  - 陷阱：`passed` 初始必须 true（曾误设 finished 导致正确路径也失败）；`Box<dyn Task>` 进静态共享需 `Send + Sync`；`tokio::fs::File::open` 需 async 包装
- **当前接线**：`DownloadOptions.check_integrity` 通过 canonical `config::OptionRegistry`、RPC task/session 映射和测试构造点解析 `--check-integrity`；BT 单文件与多文件（跨物理文件边界的 piece）以及 HTTP（DownloadContext 有 piece hashes 时，如 Metalink）均经 Rust `ci_man` 入队。FTP/SFTP 使用各自 command 的 Rust checksum 生命周期；旧的无调用方 FTP `file_preparation` 复制层已删除。
- 全量验证：**4166 passed / 0 failed / 1 ignored**（较上轮 +5），无卡死。

#### Metalink metaurl 兜底（P1 #7 修复）+ initStorage 判定为架构差异

- **metaurl（C++ BtDependency）**：`MetalinkDownloadCommand::new` 原来拒绝一切无 HTTP URL 的文件（"no download URL"），metaurl-only 文件是死路。修复：
  - `new` 接受带 `mediatype="application/x-bittorrent"` metaurl 的文件；`execute()` 无镜像成功时按 priority 逐个下载 `.torrent` → `BtDownloadCommand` 执行（`try_torrent_metaurl`，`bittorrent` feature 门控，无该 feature 回退原错误）
  - `FileDownloadInfo.torrent_metaurls` 单/多文件模式都填充
  - 陷阱：`bt_download_command` 模块本身是 `#[cfg(feature="bittorrent")]`，跨 feature 引用必须同样门控；方法曾误放入 `impl Command`（E0407），需移到 inherent impl；`try_torrent_metaurl` 需 `&mut self`（要写 self.completed）
- **#11 initStorage 判定 N/A**：`SegmentMan` 在下载路径零调用方（HTTP 直写文件、BT 用 PieceManager），与旧 PieceStorage 校验器同类的孤儿模块——给没人消费的 DiskAdaptor 做自动初始化是死功。差距文档标记 N/A。
- 全量验证：`--features bittorrent,metalink` → **4240 passed / 0 failed / 1 ignored**（metalink 模块测试随 feature 全部纳入），无卡死。

#### BT seed 期 tracker re-announce（P1 #4 修复）

- **问题**：做种时完全不向 tracker 重新 announce（C++ SeedCheckCommand 按 tracker interval 续报，让 leecher 能找到 seeder）。`new_with_choking_algo` 的 info_hash 还是全 0。
- **实现**：`BtSeedManager` 加 `announcer: Option<TrackerAnnouncer>` + `peer_id` 字段；`run_seeding_loop` 每 tick 检查 `is_default_announce_ready()` → `announce()`（状态机按 interval 节流）；新构造 `new_with_announcer`；`run_seeding_phase` 从 TorrentAttribute 取 announce list 构造 announcer + `generate_peer_id()` + 真实 info_hash（execute 调用处传 `meta.info_hash.bytes`）。
- 陷阱：`BtSeedManager` 是 bittorrent feature 门控，编译错误只在带 feature 时暴露（默认 feature check 全绿但全量崩）——**改 feature 门控模块必须用全 feature 编译验证**；`ctx.and_then(|c| c.get_attribute(...).and_then(downcast_ref))` 返回引用借用参数（E0515），必须嵌套 if-let 解包后克隆；build() 有 4 个调用点（非 3 个），漏一个就 E0061。
- 全量验证：**4240 passed / 0 failed / 1 ignored**，无卡死。

#### BEP 5 DHT Port 消息接线（P1 #2 修复）

- **问题**：真实下载路径（wait_for_piece_block / wait_for_any_piece_block）读到 Port 消息直接丢弃；`BtPeerInteraction::dispatch_message` 的 Port 分支只打日志，且该链路无生产调用方（孤儿）。C++ `BtPortMessage::doReceivedAction` 会把 (peer_ip, port) 加入 DHT。
- **实现**：两条等待循环加 `BtMessage::Port { port }` 分支 → `add_node(ip:port)`；`add_node` 同步等 ping 响应（query_timeout），必须 `tokio::spawn` 分离执行避免阻塞块下载；`dht_engine: Option<Arc<DhtEngine>>` 从 `download_pieces_loop`（&mut self）→ `download_piece_blocks(_endgame)` → `request_block(_endgame)` → 等待循环逐层透传；Port(0)/无 peer ip 忽略。
- 陷阱：endgame.rs 的 Port 分支易插错函数（request_block_endgame 无等待循环，真正的循环在 wait_for_any_piece_block）；孤儿 dispatch 链路不改。
- 全量验证：**4240 passed / 0 failed / 1 ignored**，无卡死。

#### P1 #14 判定 DONE + 工程清理（warnings 12 → 4）

- **#14 advertisePiece 审计**：BT 每完成一个 piece 已调 `BtPeerInteraction::broadcast_have()`（piece_download.rs 向所有 peer 广播 Have），正是 C++ advertisePiece 的功能等价——原 PARTIAL 是审计遗漏，标记 DONE（零代码改动）。
- **unused imports 清理**：maintenance.rs FloodingStat、choke_and_config.rs tracing::debug + BtLeecherStateChoke/BtSeederStateChoke、peer_ops.rs HashSet、multi_file_layout.rs MultiFileLayout、keepalive_flooding.rs make_test_conn、dropped.rs make_peer、bt_announce/tests.rs super::*、piece_provider.rs super::*（**piece_provider 的 MultiFileLayout 有 4 处实际使用，误删会 E0433——该删的是 super::***）；tracker_announce.rs 测试变量改 `_announcer`。
- **dead code**：coordinator.rs `segment_manager` accessor 是 test-only（4 处测试调用）→ `#[cfg(test)]` 门控（非删除）；bt_request_factory/tests.rs 测试基建加 `#![allow(dead_code)]`。
- 剩余 4 个 lib warning 全是**刻意的 deprecated 标记**（MSE 旧类型兼容层，指向新实现）。
- 全量验证：**4240 passed / 0 failed / 1 ignored**，无卡死。

#### WrDiskCache BT 接入 + 乱序写 P0 bug（P1 #3）+ RPC option 修复

- **#3 完成 + 重大发现**：BT 单文件原用 DefaultDiskWriter::write()（顺序追加），但 BT 默认 RarestFirst 乱序下载——piece 被写到错误偏移，静默文件损坏 P0 bug（web-seed 兜底同病）。修复：BT 单文件改 CachedDiskWriter（write_at(offset) 定位写 + 16MB 写回缓存），web-seed 同样改定位写；ThrottledWriter 加 SeekableDiskWriter impl（限速保留，new bound 放宽到 W: Send），Box<dyn SeekableDiskWriter> blanket impl。2 个回归测试（乱序落位验证）。
- **RPC 命名不一致 bug**（agent 审计 231 注册 vs 68 消费 163 缺口中发现）：bt-force-encryption（注册名）RPC 读 bt-force-encrypt、max-tries 读 max-retries——用户配置静默失效。双名兼容修复（注册名优先）于 addUri + changeOption 两处。
- **bt-tracker 端到端接入**：DownloadOptions.bt_tracker 字段 + apply/RPC（数组或逗号换行分隔）/session 解析 + peer_management 的 TrackerAnnouncer 用用户 tracker 覆盖 torrent announce list。
- 陷阱：python 写入含 
 的 Rust 源码时 bash 双引号会降级转义（4 反斜杠→2→真换行）——用脚本文件或 Edit 工具避免；od -c 对换行符显示 
 与字符 
 难区分，用 od -x 精确判断。
- 全量验证：**4242 passed / 0 failed / 1 ignored**（+2），无卡死。

#### RPC 全局 option 合并（changeGlobalOption 生效）

- **问题**：add_task 只用本次调用 options；changeGlobalOption 只写 global_opts（getGlobalOption 展示），从不合并进下载——C++ 语义中全局 option 是会话默认。
- **实现**：RpcEngine 加 user_global_opts（仅用户显式设置；registry 默认值在 global_opts，避免漏进下载覆盖 per-download 默认）；changeGlobalOption 双写；add_task 合并（task opts 优先）后构造 DownloadOptions。
- 字段级审计：RPC 已读全部 58 个 DownloadOptions 字段；163 个注册缺口均为引擎/全局类（max-concurrent-downloads 等），非下载级。
- 陷阱：HashMap 循环移动借用（先 &new_opts 后 move）。
- 全量验证：**4242 passed / 0 failed / 1 ignored**，无卡死。

#### 引擎级全局 option：max-concurrent-downloads 接线

- **发现**：RequestGroupMan.max_concurrent 硬编码 5——CLI -j/--max-concurrent-downloads 与 RPC changeGlobalOption 都不生效（option 只注册未消费，比 RPC 缺口更严重）。
- **修复**：CLI 启动（aria2/src/app/mod.rs run() 任务添加前）读 option 设置 man；RPC changeGlobalOption 检测 max-concurrent-downloads 发 EngineCommand::SetMaxConcurrent（engine_loop 的 reduce_to_limit 现成，自动暂停超额活动下载）。1 个接线测试（changeGlobalOption -> 命令发出断言）。
- **TODO**：max-overall-download-limit / max-overall-upload-limit 需引擎全局 RateLimiter（当前无此架构；per-download max-download-limit/max-upload-limit 已生效）。
- 全量验证：**4243 passed / 0 failed / 1 ignored**（+1），无卡死。

### 2026-08-01

#### 深度对照增量修复（request/session/unsafe/selector/rate_limiter）

本轮在上轮 P1 收官基础上，针对 Explore agent 模块对照发现的 5 类系统性缺陷进行集中修复。

##### 1. Request 调度闭环修复（工程师-1）

- **问题**：`process_task_completions` 不区分 pause 与 error/completion，暂停的任务被当作已完成而降级丢弃；pause 即毁任务不可恢复。
- **修复**：
  - `request_group_man/demotion.rs` 新增 `requeue_non_terminal_groups`：暂停组从 active 移出并放入 reserved，取消暂停后由 promotion 再次进入 active
  - `engine_loop.rs` `process_task_completions` 区分 pause/error/completion 三种终态，pause 走 requeue 而非 demote；completion ledger 按 command generation 去重，不再按 GID 粗粒度丢弃同组的其他 command
  - 手动 pause/forcePause 保持 Paused，必须显式 unpause；`reduce_to_limit()` 设置 restart 标志后允许自动恢复
  - `mark_session_dirty` 挂接 engine loop，状态变更触发会话自动保存
  - 新增 6 个测试

##### 2. Session 会话兼容修复（工程师-3）

- **问题**：GID 序列化为裸 hex（`{:x}`）不补零，C++ aria2 期望 `{:016x}` 格式，导致 C++ 无法加载 Rust 保存的 session 文件。
- **修复**：
  - `session_serialize_impl.rs` GID 改为 `{:016x}` 补零
  - 周期保存 / RPC saveSession 路径验证
  - 新增 7 个测试

##### 3. Unsafe 加固 + Token 常量时间比较（工程师-4）

- **问题**：RPC token 验证用明文 `==` 比较，存在时序侧信道；mmap 磁盘写入有 SIGBUS 风险；daemon close_fds/double-fork 缺安全约束文档。
- **修复**：
  - `server.rs` 3 处 token 比较替换为 `constant_time_eq`（P1 #27 PARTIAL → FIXED）
  - `mmap_disk_writer.rs` 添加 SIGBUS 风险文档 + 磁盘预检
  - `aria2/src/daemon.rs` close_fds/double-fork 安全约束文档
  - 新增 3 个测试

##### 4. Selector stat_man 统一（本轮完成）

- **问题**：每个 `DownloadCommand::new_with_group` 创建独立 `Arc<ServerStatMan::new()`，AdaptiveUriSelector 服务器速度统计跨下载隔离——自适应选源退化为单下载启发式，无法从历史下载中学习。
- **修复**：
  - `server_stat_man.rs` 添加 `shared()` 进程级单例（`OnceLock`，同 `FileAllocationMan::shared()` 模式）
  - 5 处创建点改用 `ServerStatMan::shared().clone()`：
    - `download_command/mod.rs` × 2（`new_with_group` + `new_with_group_and_client`）
    - `concurrent_download/pipeline.rs` × 3（AdaptiveUriSelector、ConcurrentSegmentManager、MirrorCoordinator）
  - 修改文件：`server_stat_man.rs`、`download_command/mod.rs`、`concurrent_download/pipeline.rs`

2026-08-14 进一步修正并发镜像反馈：成功速度、失败状态和连接数重平衡
统一通过 `extract_host_and_protocol` 使用 `(hostname, protocol)` 键，避免
HTTP/HTTPS 同 hostname 写入 host-only 统计后无法被协议化选择器读取。该轮
`concurrent_segment_manager` 23 项、`mirror_coordinator` 11 项测试通过；DNS
坏地址完整生命周期仍记录在当前兼容矩阵的 `部分` 状态中。

##### 5. 全局限速器接线（本轮完成）

- **问题**：`DownloadEngine.global_limiter` 字段存在但从未传递到下载路径——`max-overall-download-limit` / `max-overall-upload-limit` 是死 option。每下载限速（`max-download-limit`）已生效，但全局聚合限速缺失。
- **修复**（14 文件）：
  - `throttled_writer.rs`：添加 `global_limiter: Option<RateLimiter>` 字段 + `with_global_limiter()` builder；`write()` / `write_at()` / `write_bytes_at()` 先 acquire per-download 再 acquire global（串行双 bucket）
  - `download_command/mod.rs`：添加 `global_limiter` 字段 + `set_global_limiter()` setter
  - `task_spawner.rs`：`spawn_download_task` + `create_command_for_uri` 加 `global_limiter` 参数，HTTP 路径调 `cmd.set_global_limiter()`
  - `engine_loop.rs` + `lifecycle.rs`：`EngineLoopContext` 透传 `global_limiter` clone
  - `sequential_download/mod.rs` + `download_flow.rs` + `gap_download.rs`：SequentialDownloader 加 `global_limiter` 字段，ThrottledWriter 创建时 `with_global_limiter()`，gap 下载 acquire 点加全局 acquire
  - `concurrent_download/mod.rs` + `segment.rs`：ConcurrentDownloader 加 `global_limiter` 字段，5 处 acquire 点加全局 acquire
  - `execute.rs`：3 处 `SequentialDownloader::new` / `ConcurrentDownloader::new` 传 `self.global_limiter.clone()`
  - **BT/FTP/SFTP/Metalink/Magnet 路径已补全**（见下文「兼容性收口增量」B 节）
  - `RateLimiter` 是 `#[derive(Clone)]`，clone 共享 `Arc<RateLimiterInner>`（两个 TokenBucket），所以 clone 廉价

##### 验证结果

- `cargo check --workspace --features aria2-core/bittorrent`：0 error，4 warnings（均为 deprecated MSE 兼容层）
- `cargo test --workspace --features aria2-core/bittorrent,aria2-core/metalink --lib`：
  **4257 passed / 0 failed / 1 ignored**（较上轮 4243 +14），全程 43 秒无卡死
- 测试分布：aria2 32 / aria2-core 3295 / aria2-protocol 732 / aria2-rpc 198

##### P1 状态总结

28 项 P1 中 **26 项闭环**（DONE/FIXED/N/A），仅剩 2 项 aria2-next 增强项（spdlog 日志轮转、Content-Disposition 边角已修复但表格可能未更新）——这些非 aria2_original 兼容必需。**实质性功能缺口清零。**

##### 兼容性收口增量（本轮续）

在上述 2026-08-01 会话基础上，继续完成 3 项收口修复：

**A. Content-Disposition 尾部 `;` 接受（用户决策：超越原版）**

- **问题**：C++ aria2 的终态 `switch(state)` 不接受 `CD_BEFORE_DISPOSITION_PARM_NAME`，拒绝以 `;` 结尾的 Content-Disposition 头——这是已知 bug（GitHub issue #1118，挂 5+ 年），破坏 S3/CloudFront/nginx 下载。
- **用户决策**：不继承 C++ 已知 bug，按 RFC 6266 `*( ";" disposition-parm )` 语法接受尾部 `;`。
- **修复**：`parser.rs` 终态 match 第一臂加入 `ParseState::BeforeParmName`；中间空参数（`attachment; ;filename=foo`）仍被 `BeforeParmName` 状态处理器拒绝（非 token 字节触发 `return None`）。7 个 trailing-`;` 测试从 `_rejected` 翻为 `_accepted`，共 110 测试。
- **文档修正**：移除虚构状态名 `CD_VALUE_COMPLETE` / `CD_FINAL_EMPTY_PARAMETER_ALLOWED`（这些是文档作者自造，C++ 中不存在）。

**B. 全局限速器补全到所有下载路径**

- **问题**：上轮仅接通 HTTP 路径（6 acquire sites），BT/FTP/SFTP/Metalink 路径留 TODO。
- **修复**（14 文件）：
  - 6 个命令结构体添加 `global_limiter: Option<RateLimiter>` 字段 + `set_global_limiter()` setter：
    `bt_download_command/mod.rs`、`bt_download_command/constructor.rs`、`ftp_download_command/types.rs`、`sftp_download_command/types.rs`、`metalink_download_command/mod.rs`、`magnet_download_command.rs`
  - 注入点：`task_spawner.rs`（BT/FTP/SFTP）、`engine.rs`（Metalink）、`magnet_download_command.rs` + `metalink_download_command/execution.rs`（转发）
  - 4 处 ThrottledWriter 创建点统一模板：per-download limiter + `with_global_limiter()` 双 bucket 串行 acquire
  - `RateLimiter` 是 `#[derive(Clone)]`，clone 共享 `Arc<Inner>`

**C. SFTP `FileOpError` 类型缺失修复**

- **问题**：`sftp_download_command/types.rs:20` import `FileOpError` from `aria2_protocol::sftp::file_ops`，但 `file_ops.rs` 从未定义此类型——所有方法返回 `Result<_, String>`。导致 `cargo build --features sftp` 编译失败（E0432），SFTP 全局限速代码无法验证。
- **修复**：
  - `file_ops.rs` 新增 `FileOpError` 枚举（`NotFound`/`PermissionDenied`/`Network`/`Other` 变体）
  - `impl From<String>` 解析 SFTP 状态码分类（code=2→NotFound, code=3→PermissionDenied, code=6/7→Network）
  - `impl Display` 格式化
  - `execution.rs` 3 处 call site 转换 `String` → `FileOpError` 后传入 `map_file_op_error`

**D. 文档矛盾修正**

- DHT P1 #1 "not wired" 标 TODO 与 P1 #17 "DONE" 冲突 → 统一为 DONE
- `CD_VALUE_COMPLETE` / `CD_FINAL_EMPTY_PARAMETER_ALLOWED` 虚构状态名 → 移除
- 全局限速 #30 "BT/FTP/SFTP/Metalink left as TODO" → 更新为 all paths wired
- 新增 Fixed #34（CD trailing `;`）+ #35（SFTP FileOpError）

#### 本轮修复（阶段 1：编译恢复 + 阻塞性缺陷）

| 项 | 文件 | 说明 |
|---|---|---|
| **PiecePicker 从桩实现为真实选择器** | `aria2-protocol/src/bittorrent/piece/picker.rs` | 原 `select()` / `pick_next()` 恒返回 `None`，导致 **BT 下载完全选不出分片**（P0 功能失效）。重写为完整实现：7 种 `ScanOrder`（Forward / Backward / Rarest / Random / LongestRun / Priority / Geometric）、head/tail 游标使顺序选择摊销 O(1)、endgame 模式（阈值默认 20，可 `set_endgame_threshold` 调整）、`mark_in_progress` / `is_in_progress` / `is_completed` / `set_priority`。`remaining_count()` 与 `is_complete()` 由 O(n) 改为 O(1)。随机源用内置 xorshift64\*，不引入 rand 依赖。新增 25 个单测，picker 共 35 测试全绿 |
| **DHT 引导异步化（修复测试卡死）** | `aria2-protocol/src/bittorrent/dht/engine.rs` | `DhtEngine::start()` 原先 `await` 完整公网 bootstrap，导致 6 个测试各卡 60s+。改为 `spawn_bootstrap()` 后台任务 + `tokio::time::timeout`（默认 60s）；新增 `DhtEngineConfig::local()`（`port: 0`、`bootstrap_on_start: false`）供测试使用。语义上与 C++ aria2 一致——原版 bootstrap 也是事件循环里的异步命令，不阻塞启动 |
| DHT 测试配置统一 | registry / magnet / integration / engine 自身共 11 处 | `DhtEngineConfig { port: 0, ..Default::default() }` → `DhtEngineConfig::local()` |
| `verify_invariant` 可见性 | `aria2-core/src/engine/bt_peer_storage/storage/choke_and_config.rs` | `pub(super)` 覆盖不到同模块 `tests/` 子目录的 24 处调用，改为 `pub(in crate::engine::bt_peer_storage)` |
| bench 字段缺失 | `aria2-core/benches/p2_bench.rs` | BtProgress 补 `upload_length` / `in_flight_pieces` / `is_torrent` |
| **bencode 解码器加固（栈溢出 DoS）** | `aria2-protocol/src/bittorrent/bencode/codec.rs` | 递归下降解码器**无嵌套深度上限**，恶意 `.torrent`（长串 `l`/`d` 开括号）驱动无界递归 → 栈溢出，Rust 无法 catch，直接 abort 进程。已对齐 C++ `BencodeParser::pushState` 的 50 层限制，新增 `MAX_NESTING_DEPTH` 与 `decode_at_depth`（公开 API `decode()` 签名不变）。同时把字节串长度前缀改为 `checked_add`——`usize::MAX` 长度会绕过 `data_end > bytes.len()` 检查并在切片索引处 panic。新增 5 个回归测试（含 200 000 层嵌套输入） |
| 测试导入错误 | `aria2-core/tests/test_e2e_session.rs` | `deserialize_entries` 不存在 → `session_serializer::deserialize` |
| **分块校验哈希算法硬编码** | `aria2-core/src/checksum/check_integrity/validator.rs` | `PieceHashValidator::compute_hash` 硬编码 SHA-1，无视 `DownloadContext::get_piece_hash_type()`。Metalink `<pieces type="sha-256">` 或 HTTP sha-256/sha-512 分块哈希会拿 40 字符摘要去比对 64 字符期望值，**每一块都判失败**并触发无休止重下。现按上下文声明的算法走 `MessageDigest`；未知算法显式失败而非静默回退到 SHA-1；顺带把重复的推进逻辑抽成 `advance()`，保证所有退出路径都推进游标不会卡住迭代。5 个回归测试 |
| 台账清理 | `docs/migration/_util_p1.md` | 早期分片草稿（54 单元）经比对确认为 `util.md`（109 单元）的子集，且 20 处状态判定分歧中 util.md 均正确（已复核 `Dependency` 确有实现、`SequentialPicker` 实为泛型 FIFO 模板）。唯一有价值的发现——bencode 深度 DoS——已修复并回写 util.md，分片文件删除 |
| 台账判定更正 | `docs/migration/checksum.md` | 2 项误判为 `缺失` 的 checksum 单元经代码复核改判 `部分`（详见上方说明），合计 `缺失` 由 7 降为 5 |

验证结果：

- `cargo check --workspace --all-targets` 源码零错误
- `cargo test --workspace --features aria2-core/bittorrent --lib`：
  **4120 passed / 0 failed / 1 ignored，全程 38 秒**
- DHT 相关 197 个测试由「4 个各卡 60s+」变为 **0.29 秒全绿**

> Windows 环境注意：`target/.fingerprint/*/invoked.timestamp` 偶发
> "拒绝访问 (os error 5)" 是杀软实时扫描的瞬时文件锁，**非源码错误**，重试即过。
> 若 `target/debug/.cargo-build-lock` 进入"挂起删除"态导致 cargo 无法启动，
> 在命令前加 `rm -f target/debug/.cargo-build-lock` 即可（该锁在 cargo 退出时本就应被删除）。

#### 本轮修复（阶段 2：`缺失` 项消项 + 停机语义对齐）

| 项 | 文件 | 说明 |
|---|---|---|
| **优雅停机在 RPC 模式挂死（真实缺陷）** | `aria2-core/src/engine/engine_loop.rs` | `halt_requested` 此前是 **write-only** 标志——`aria2.shutdown` 与 Ctrl+C 都会置位，却从未被任何退出条件读取。唯一的退出判据是 `all_done && !ctx.keep_alive`，而 RPC 模式下 `keep_alive == true` 恒成立，于是**优雅停机永远不会退出**，只能靠强制杀进程。修复：新增 `graceful_done = halt_requested && running_downloads.is_empty()` 退出分支；同时补齐提升门控——halt 后不再把 reserved 组提升为活动组（对齐 C++ `FillRequestGroupCommand::execute()` 在 `isHaltRequested()` 时的早退）。新增 5 个回归测试（keep-alive 下的优雅/强制/Ctrl+C 三条退出路径 + 一个"未 halt 不退出"反例 + 非 keep-alive 空闲退出），全部带超时预算防挂死 |
| **`--stop=N` / `--stop-with-process=PID` 实现并接线** | 新增 `aria2-core/src/engine/halt_watchers.rs`；`aria2/src/app/engine.rs` | 对应 C++ `TimedHaltCommand` / `WatchProcessCommand`。`spawn_timed_halt` 用 `tokio::time::sleep` 与 `cmd_tx.closed()` 竞速（引擎先退出时看守任务自行结束，不泄漏）；`spawn_process_watch` 按 C++ 的 1s 周期轮询 `is_process_alive`。存活判定：Windows 走 `OpenProcess(PROCESS_SYNCHRONIZE)` + `WaitForSingleObject(h, 0) == WAIT_TIMEOUT`（句柄打不开即视为已退出，与 C++ 一致），Unix 走 `kill(pid, 0)` 且 `EPERM` 视为存活。CLI 在引擎启动时按选项 spawn，force 默认 `false`（对齐 C++ 默认 `forceHalt=false`）。8 个单测，用 `start_paused = true` + `time::advance` 推进时钟，零真实等待 |
| **RPC shutdown 响应被抢先截断** | `aria2-rpc/src/handlers/task.rs` | `handle_shutdown` 原先内联发 `HaltAll`，会在 JSON-RPC 响应 flush 前就拆掉承载该连接的引擎，客户端可能收到断连而非 `OK`。改为复用 `spawn_timed_halt(.., RPC_SHUTDOWN_GRACE = 3s, ..)`，对齐 C++ 的 `goingShutdown` 宽限窗口；`forceShutdown` 同理，仅 `force` 标志不同 |
| **`haves` 队列无界增长隐患** | `aria2-core/src/segment/piece_storage/default_storage/piece_ops.rs` | Rust 的 have 广播已改为集中式直推，`haves` 公告板退化为无人清扫的死代码队列，每完成一个分片就追加一条且永不回收。在唯一插入点 `advertise_piece` 内联 5s TTL 驱逐（`HAVE_ENTRY_TTL_MS`，对齐 C++ `removeAdvertisedPiece(now - 5s)` 的窗口），首元素未过期时直接跳过 retain，避免每次插入都做全量扫描。这比 C++ 的独立定时命令更难写错——插入与回收在同一处，不可能只加不删。2 个专项回归测试（注入过期条目验证驱逐 / 验证新鲜条目不被误删） |

验证结果（阶段 2）：

- `cargo check -p aria2-core --lib`：0 error
- `halt_watchers` 8 测试全绿、`engine_loop` 5 测试全绿
- `cargo test -p aria2-core --lib piece_storage`：**89 passed / 0 failed，0.01 秒**
- `aria2` / `aria2-rpc` 编译 0 error

### 2026-08-07

#### RPC 启动写锁阻塞与异步锁生命周期收口

- **根因**：`aria2/src/app/mod.rs` 在计算是否存在恢复任务后，把
  `RequestGroupMan` 的读 guard 保留到了整个 `App::run` 生命周期。RPC
  `addUri` 的首个写锁因此永久等待。
- **修复**：恢复任务数量改为短生命周期快照；应用层会话保存、Metalink
  GID 分配、引擎 remove/force-halt 和 timeout housekeeping 均不再持有
  manager guard 跨越异步等待。RPC add path 保持单一注册，不重复发送同一
  `EngineCommand::AddDownload`。
- **回归结果**：Node HTTP E2E 3/3、pause/resume 2/2，完整 Node E2E
  11/11；Node unit 86/86；`aria2-rpc` lib 201/201；默认
  `aria2-core` lib 2428 passed、1 ignored；all-features workspace lib
  matrix 通过。
- **环境限制**：Python binding 测试尚未执行。当前虚拟环境的 launcher
  指向不存在的 Python 安装，需要修复解释器后再纳入 acceptance gate。

### 2026-08-08

#### Engine lifecycle and shared rate-limit seam

- `task_spawner::spawn_download_task` now returns synchronously after creating
  a tracked Tokio task. DNS resolution and protocol command construction stay
  inside that task, so the engine loop can continue processing lifecycle
  commands while setup is waiting.
- The shutdown seam is now a `CancellationToken` shared by the task and
  `RunningDownload`; removal, force halt, timeout cleanup, and final teardown
  all use the same cancellation path and bounded join wait.
- `EngineCommand::SetGlobalRateLimit` updates one shared `RateLimiter` and the
  `RequestGroupMan` reporting snapshot. The regression test covers both views,
  including contexts that had no limiter before the runtime update.
- The current ownership decisions and remaining duplication work are recorded
  in the `Architecture And Duplication Register` in
  `docs/compatibility-status.md`. The DHT protocol crate remains canonical;
  HTTP/FTP transport layers are deliberately not collapsed until live
  behavior comparison proves which adapter owns each responsibility.

#### Verification checkpoint

- `cargo fmt --all -- --check`, core all-feature check, and workspace
  all-target/all-feature Clippy with `-D warnings` passed.
- Package-level all-feature suites passed: aria2-protocol 872, aria2-rpc 371
  (0 failed), aria2 254. Core executed 3,411 tests with 11 ignored and 0 failed before a
  Windows 600-second aggregate command timeout; the last BitTorrent target
  was then run separately with 21 passed and 2 ignored.
- Node.js typecheck/build and full binding suite passed (123/123). Python
  binding tests passed (136/136) using the bundled Python runtime plus an
  isolated temporary dependency directory and a real `aria2c` binary.
- The one-shot `cargo test --workspace --all-features -j 1` command did not
  reach its test phase before the Windows build timeout. It is therefore not
  recorded as a green workspace aggregate run.

#### Acceptance status

This checkpoint closes the lifecycle/rate-limit regression but does not close
the migration. A source-level opaque-handle C ABI is now present in
`aria2-core/src/c_api.rs` and `bindings/c/`; it is not a binary-compatible
replacement for the original C++ classes and STL ABI. Metalink now owns GID
allocation and preserves the manager-owned source/base URI while building
metadata/payload graphs. Its explicit metadata-success,
direct-mirror-fallback, and terminal-failure states are covered by focused
tests. Same-metaurl multi-file grouping, full `follow-torrent=mem` semantics,
session graph restoration, integrity lifecycle TODO/no-op paths, live
SFTP/FTP/DHT/BitTorrent Metalink interoperability, complete CLI/RPC
original-client comparison, duplicate transport ownership, ignored network
tests, and the aria2 C++ performance baseline remain open in
`docs/compatibility-status.md`.

#### C API and Metalink verification checkpoint

- `cargo fmt --all -- --check` passed after the FFI safety-contract cleanup.
- `cargo clippy -p aria2-core --all-targets --all-features -- -D warnings`
  passed. The C ABI entry points document pointer ownership and nullability;
  no lint is disabled for the new interface.
- `cargo test -p aria2-core --all-features c_api --lib` is the focused C API
  lifecycle/control regression command. It covers library/session lifecycle,
  asynchronous queue observation, and stop-state polling.
- Metalink graph tests cover manager-owned GID allocation, metadata-to-payload
  dependency direction, direct URI fallback, metadata parse failure without a
  fallback, relative URI base propagation, and the absence of payload
  self-locking through dependency storage.

#### RPC compatibility seam and option parsing checkpoint

- Compared the RPC option mutation path with
  `aria2_original/src/RpcMethod.cc` and
  `aria2_original/src/RpcMethodImpl.cc`: unknown and non-changeable keys are
  ignored; selected option-handler parse failures are execution errors with
  code `1`, rather than JSON-RPC `-32602`.
- The original 120-name `setChangeGlobalOption(true)` policy and the
  task-level active/reserved `changeOption` policy are centralized in
  `aria2-core/src/config/runtime.rs`; the RPC crate no longer owns a second
  whitelist, and `request_group` only preserves the historical re-export
  path for callers.
- JSON value normalization and typed string/size/integer/boolean/enum parsing
  live in core. Enum choice sets are attached to `OptionDef`, so config, RPC,
  and C API paths share the same validation seam.
- Regression coverage includes core no-partial-update tests, handler tests,
  and real HTTP E2E checks for invalid `changeOption` and
  `changeGlobalOption` values.
- `cargo test -p aria2-rpc --all-features --tests -- --test-threads=1`:
  **395 passed / 0 failed**. This proves the RPC test scope only; the
  workspace aggregate and complete original browser-client interoperability
  matrix remain open.

#### HTTP resume and mirror failover checkpoint (2026-08-09)

- `DownloadCommand` now owns the command-generation policy for multiple HTTP
  URIs. A `CannotResume` response is recorded, the next mirror is tried in
  order, and `max-resume-failure-tries` can trigger the original fresh-download
  fallback before the mirror list is exhausted.
- `SequentialDownloader` reports the typed `CannotResume` result and no longer
  decides whether to delete control state or restart the task. Resume writes
  begin at `start_offset`, and a rejected request restores the meaningful file
  length after preallocation so a preallocated tail cannot be reported as
  completed data.
- Focused evidence: `cargo test -p aria2-core --test test_e2e_download
  --all-features -- --test-threads=1` (26 passed, 0 failed, 2 ignored), with
  four dedicated resume/failover tests passing.
- This closes the sequential HTTP resume policy gap only. Concurrent range
  control-file recovery, FTP/SFTP parity, and original-client interoperability
  remain open and are not claimed as complete.

#### RPC wire compatibility seam (2026-08-09)

- `aria2_original/src/rpc_helper.cc` and
  `aria2_original/src/HttpServerBodyCommand.cc` define the wire contract for
  the HTTP and WebSocket JSON-RPC adapters. The Rust server keeps a separate
  `parse_aria2_wire_document` seam so external envelope behavior is not mixed
  with the typed internal `JsonRpcRequest` helper.
- Covered original rules: ignore `jsonrpc`, default missing `params` to `[]`,
  reject missing `id` or object params before method dispatch, materialize
  object-level errors inside batches, skip non-object batch items, and preserve
  empty batches as `[]`.
- The legacy GET/JSONP adapter also preserves the original empty-`params=`
  omission rule. Basic Auth treats an empty `rpc-passwd` as unset, so any
  password is accepted after the configured username, matching the original
  `HttpServer::setUsernamePassword` behavior.
- CORS follows the original opt-in rule: the default configuration emits no
  CORS headers, while `rpc-allow-origin-all=true` uses an explicit wildcard
  configuration. The preflight cache value remains `1728000` as in
  `aria2_original/src/HttpServerBodyCommand.cc`.
- `getSessionInfo` was compared with
  `aria2_original/src/DownloadEngine.cc` and
  `aria2_original/src/RpcMethodImpl.cc`: Rust now generates one 20-byte random
  session key per `RpcEngine` and returns its 40-character lowercase
  hexadecimal form. JSON-RPC, XML-RPC, and WebSocket dispatch share that
  engine-owned value, while the old `rpc_helpers::generate_session_id` path is
  retained as a forwarding export rather than a second generator.
- Verification: `cargo test -p aria2-rpc --all-features --tests
  -- --test-threads=1` passed **395 tests / 0 failed**; RPC Clippy and format
  checks also passed. Browser-extension and complete original-client
  interoperability remain open acceptance items.

- `aria2.getServers` now follows the source-backed active-only contract from
  `aria2_original/src/RpcMethodImpl.cc`: waiting, paused, stopped, and unknown
  GIDs return execution error code 1, while active results include only real
  in-flight requests and never synthesize servers from configured mirrors.

#### Current-tree RPC client interoperability checkpoint (2026-08-13)

The current tree now has reproducible process-level evidence for the public
client seams. AriaNg's `system.multicall` refresh shape was sent to a live
`aria2c` process with one `token:` per sub-call and the expected nested result
arrays. A live XML-RPC client authenticated at `/rpc`, received `text/xml`
responses with string statistics, and shut the process down. A live WebSocket
client authenticated at `/jsonrpc`, correlated request IDs, received start/stop
notifications for the same GID, recovered after an oversized-request parse
error, and reconnected after a clean close.

Current-tree verification:

~~~text
cargo test -p aria2-rpc --all-features --tests -j 1 -- --test-threads=1
  404 passed, 0 failed
cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings
  PASS
cargo test -p aria2 --test e2e_arianng_rpc_client --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2 --test e2e_xmlrpc_client --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2 --test e2e_websocket_rpc_client --all-features -- --test-threads=1
  3 passed, 0 failed
~~~

This closes only the tested wire/client slices. The complete Chrome/browser
extension matrix, broader original-client workflows, notification ordering and
reconnect breadth, and complete XML-RPC method/error coverage remain acceptance
work; RPC and WebSocket therefore remain `PARTIAL`.

#### Request-group identity seam and XML-RPC HTTP contract (2026-08-09)

- `RequestGroupMan` now keeps a canonical GID index for every non-terminal
  group. `active` and `reserved` remain scheduling stores; moving a group
  between them no longer creates a lookup gap for RPC/C API callers. The index
  is removed only when the group is demoted, removed, or fails permanently.
- Query snapshots preserve the externally observable active-first and reserved
  FIFO order. A group seen only in the canonical index during a transfer is
  appended as a de-duplicated complement, so the identity fix cannot reorder
  `tellWaiting` or hide a task during the handoff.
- The manager regression test exercises lookup during both halves of an
  active/reserved transfer. `cargo test -p aria2-core --lib --tests
  --all-features -- --test-threads=1` completed with exit code 0; its library
  target reported 3,278 passed and 1 ignored, and all listed integration and
  performance targets completed without failures.
- XML-RPC was compared with
  `aria2_original/src/HttpServerBodyCommand.cc`: parser/value failures return
  HTTP 400 with an empty body and no `Content-Type`; successfully parsed method
  execution failures return HTTP 200 with `faultCode=1`. The E2E regression is
  `e2e_xmlrpc_parse_errors_match_original_http_contract`.

#### XML-RPC parameter coercion checkpoint (2026-08-09)

- Compared `aria2_original/src/XmlRpcRequestParserStateImpl.cc` and
  `test/RpcHelperTest.cc` with the Rust XML-RPC adapter. The original parser
  preserves explicit string contents and represents `<double>` request values
  as strings; Rust now keeps those semantics at the XML-to-RPC conversion
  seam while retaining typed `Double` values for Rust-side response building.
- Added regression coverage for leading/trailing string whitespace and nested
  option-map double conversion. The focused target
  `cargo test -p aria2-rpc --all-features --lib xml_rpc -- --test-threads=1`
  reports **15 passed / 0 failed**.
- Re-ran `cargo test -p aria2-rpc --test test_e2e_http_server --all-features
  -- --test-threads=1` with **43 passed / 0 failed**, plus RPC Clippy with
  `-D warnings`. The overall RPC status remains `PARTIAL` because complete
  original-client and browser-extension interoperability is still unverified.

#### CLI short-option compatibility checkpoint (2026-08-15)

- `aria2_original/src/OptionHandlerFactory.cc` is the source of truth for the
  short flags. The Rust registry and clap adapter now agree on the original
  mappings, including `-a file-allocation`, `-p ftp-pasv`, `-P
  parameterized-uri`, `-R remote-time`, `-u max-upload-limit`, and `-Z
  force-sequential`; `-h` invokes help, `-v` invokes version, and `-V` enables
  `check-integrity`.
- `--verbose` remains available as a Rust extension without taking `-v`.
  Rust also keeps documented additive aliases for `listen-port`, RPC options,
  and BitTorrent seeding, peer-limit, and encryption options. They do not
  replace or alter any original short option.
- `check-integrity` carries `short_name = Some('V')` in the core
  `OptionRegistry`, and `file-allocation` defaults to `prealloc` in both the
  registry and the shared runtime constant. HTTP, FTP, BitTorrent, and
  Metalink constructors read that same constant when no explicit allocation
  option is supplied.
- Focused verification: `cargo test -p aria2 --test test_cli_options
  --all-features` passed 109 tests; `cargo test -p aria2-core --all-features
  --lib config::option -- --test-threads=1` passed 45 tests; `cargo fmt --all
  -- --check` passed.
- The all-features option inventory is explicit: 214 names in
  `aria2_original/src/prefs.cc`, 212 original names in the Rust registry,
  `help`/`version` as CLI actions, and 22 additional Rust extension options.
  The eight additive short aliases are `-L` -> `listen-port`, `-e` ->
  `enable-rpc`, `-r` -> `rpc-listen-port`, `-I` -> `rpc-secret`, `-G` ->
  `seed-time`, `-g` -> `seed-ratio`, `-B` -> `bt-max-peers`, and `-X` ->
  `bt-force-encryption`. They are not presented as original registrations.
- The source metadata audit also corrected the original deprecated markers for
  `rpc-user` and `rpc-passwd`; the Rust-owned hidden coefficient options remain
  documented separately from original hidden preferences.
- Full default/changeability comparison, optional-argument/getopt edge cases,
  version/help text parity, and complete original-client interoperability
  remain open. The original parser does not dynamically generate arbitrary
  `--no-*` aliases; Rust's extra explicit aliases are documented extensions.

#### Rust-only public tracker catalog checkpoint (2026-08-12)

The public tracker catalog remains an explicitly additive `aria2-rust`
extension. It is connected to the BitTorrent announce path without changing
the original CLI/RPC wire contract: private torrents never receive catalog
entries, disabled catalogs expose no available trackers, and enabled catalogs
preserve the current snapshot across configuration changes. Source snapshots
are merged with URL de-duplication; HTTP/HTTPS/UDP entries are dispatched
through their respective announce paths, while unsupported WSS entries are
retained for parsing but are not injected into the current BT engine path.

The catalog now uses one shared crate-local rustls provider initializer with the
standalone HTTP adapter. Tracker health is kept separately from the catalog
snapshot: failed announces use exponential backoff capped at one hour, while a
successful announce clears the failure state and makes the tracker available
again. Focused verification:

~~~text
cargo test -p aria2-protocol --lib bittorrent::tracker::public_list --all-features -- --test-threads=1
15 passed, 0 failed
cargo clippy -p aria2-protocol --all-targets --all-features -- -D warnings
PASS
cargo fmt --all -- --check
PASS
~~~

This checkpoint does not claim live public-tracker availability or complete
BitTorrent/original-client interoperability. Those remain acceptance work.

#### Exact hidden option spelling checkpoint (2026-08-13)

The hidden optimization options now use the exact case-sensitive names from
`aria2_original/src/prefs.cc`: `optimize-concurrent-downloads-coeffA` and
`optimize-concurrent-downloads-coeffB`. Their defaults are `5.0` and `25.0`,
respectively. The former Rust-only lowercase spellings are not accepted as
aliases, because accepting them would change the original CLI/config/RPC
option contract.

The product version remains owned by `aria2-rust`; compatibility covers the
`aria2c` entry point and external wire/configuration behavior, not upstream
version-report text or C++ implementation identity.

#### Application RPC CORS default checkpoint (2026-08-09)

- The application-level seam now keeps `rpc-cors-domain` unset by default,
  matching `aria2_original`: starting RPC without an explicit CORS option does
  not emit `Access-Control-Allow-Origin` for an OPTIONS request.
- The regression uses `App::start_rpc_server` and a real HTTP request rather
  than only testing `CorsConfig` in the RPC crate:
  `cargo test -p aria2 --lib application_rpc_does_not_enable_cors_by_default
  --all-features -- --test-threads=1` reports **1 passed / 0 failed**.
- Explicit `rpc-allow-origin-all` and `rpc-cors-domain` remain additive opt-in
  paths. The RPC status remains `PARTIAL` until original browser extensions and
  the full external-client matrix are exercised.

#### BitTorrent DHT periodic lookup checkpoint (2026-08-14)

The periodic BitTorrent DHT lookup now follows the original command's two
distinct peer observations while keeping the implementation Rust-owned. The
active connection count selects the same 15-minute, 5-minute, 1-minute, and
5-second retry intervals as `aria2_original/src/DHTGetPeersCommand.cc`. The
retry and max-peer decision uses `DefaultPeerStorage::count_all_peers()`, the
Rust equivalent of C++ `PeerStorage::countAllPeer()`, and is committed only
after returned peers pass normal connection and storage admission. Pending
lookup tasks are cancelled and joined during command shutdown; `Drop` remains
the synchronous abort fallback.

Focused verification from the current tree:

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

This closes the periodic lookup scheduling and local lifecycle regression
slice only. Public-network DHT behavior, original-client interoperability,
full BitTorrent scheduler and seeding parity, and workspace acceptance remain
open; DHT, BitTorrent, and the overall migration remain `PARTIAL`.

#### BitTorrent DHT engine lifecycle checkpoint (2026-08-14)

- `aria2-protocol/src/bittorrent/dht/engine.rs` now owns all background task
  handles for receive, periodic maintenance, and bootstrap. A shared Tokio
  `watch` signal reaches every task; normal async shutdown waits for
  cooperative exit, aborts only tasks still blocked in network work, and
  joins every handle before routing-table persistence.
- `shutdown()` immediately exposes `ShuttingDown` through both `state()` and
  `stats()`. The change is internal Rust ownership/lifecycle work and does not
  alter user configuration, defaults, product version, or DHT wire behavior.
- The former `aria2-core/src/dht/` implementation was removed after an
  independent source/dependency audit confirmed that production code and tests
  use only `aria2-protocol/src/bittorrent/dht/`. This is duplicate-code cleanup,
  not a compatibility-layer change; configuration, defaults, product identity,
  and DHT wire behavior are unchanged.

Focused verification:

~~~text
cargo test -p aria2-protocol --features bittorrent --lib bittorrent::dht::engine::tests -- --test-threads=1
  7 passed, 0 failed
cargo test -p aria2-core --test dht_integration_tests --features bittorrent -- --test-threads=1
  30 passed, 0 failed, 4 ignored
cargo check -p aria2-core --features bittorrent --lib
  PASS
cargo clippy -p aria2-protocol --all-targets --features bittorrent -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

## 2026-08-16 FTP/SFTP remote not-found checkpoint

FTP 550 responses from CWD traversal, `SIZE`, and `RETR` now map to the
typed `ResourceNotFound` result instead of an unknown fatal error. The FTP
command records remote not-found responses in the owning `RequestGroup`,
honors both `max-file-not-found` and the public total-attempt `max-tries`
limit, and preserves distinct permission and transport error classes. SFTP
`SSH_FX_NO_SUCH_FILE` from remote file operations follows the same typed result
and bounded not-found retry path.

Rust-owned verification:

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
status matrices, cross-protocol lifecycle E2E, original-client
interoperability, and final workspace acceptance remain open. FTP, SFTP, and
the overall migration therefore remain `PARTIAL`.

## 2026-08-16 Metalink not-found retry checkpoint

Metalink owns its direct-mirror and torrent-metaurl loops instead of routing
their HTTP responses through the ordinary HTTP response command. The loops now
apply the owning `RequestGroup`'s not-found counter to `ResourceNotFound`
responses and stop with `MaxFileNotFound` at the configured zero-progress
threshold. Transport, server, and other protocol failures retain their
existing mirror or metaurl failover behavior.

Rust-owned verification:

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
cross-protocol lifecycle E2E remain open. Metalink and the overall migration
therefore remain `PARTIAL`.
