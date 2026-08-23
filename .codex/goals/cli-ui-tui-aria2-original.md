# CLI/UI/TUI Compatibility Matrix

Reference implementation: `aria2_original` at `dc89cd3dac031bda6ba34ba6b7b69eba6862c9bd`
Branch under test: `ui`
Binary identity: `aria2` 0.3.3
Library identities: `aria2-core` 0.3.3, `aria2-protocol` 0.3.2, `aria2-rpc` 0.3.2

This matrix records the user-visible CLI and console UI scope from the goal
objective. Protocol implementation status remains governed by
`docs/compatibility-status.md`; this file does not turn protocol gaps into UI
claims.

| Domain | aria2_original source | Rust implementation | Evidence | Status |
|---|---|---|---|---|
| Version and help identity | `src/usage_text.h`, `src/help_tags.cc` | `aria2/src/app/cli.rs`, `aria2/src/identity.rs` | `cargo test -p aria2 --test test_cli_options` | PASS |
| Short/long options and aliases | `src/OptionParser.h`, `src/option_processing.cc` | `aria2/src/app/cli.rs`, `aria2/src/app/config.rs` | CLI regression option inventory, type, alias, and validation tests | PASS |
| Config files and precedence | `src/option_processing.cc`, `src/prefs.h` | `aria2/src/app/config.rs` | `cargo test -p aria2 --test test_config_file` | PASS |
| Quiet, color, stderr, summary interval | `src/console.cc`, `src/prefs.h` | `aria2/src/ui/console_progress.rs`, `aria2/src/ui/progress_bar.rs` | `cargo test -p aria2 --test e2e_cli_ui -- --nocapture` | PASS |
| Non-TTY output and ANSI hygiene | `src/console.cc` | `aria2/src/ui/console_progress.rs`, `aria2-core/src/log.rs` | CLI E2E asserts plain output; file logging disables ANSI | PASS |
| Completion/error summaries | `src/ConsoleStatCalc.cc`, `src/console.cc` | `aria2/src/ui/progress_bar.rs`, `aria2/src/app/mod.rs` | CLI E2E asserts per-task and aggregate complete/error summaries | PASS |
| Multiple task state projection | `src/ConsoleStatCalc.cc` | `aria2-core/src/request/request_group/status_snapshot.rs`, `aria2/src/ui/console_progress.rs` | CLI multi-task E2E plus core lifecycle tests | PASS |
| Resize and terminal-width adaptation | `src/console.cc`, `src/ConsoleStatCalc.cc` | `aria2-core/src/ui.rs`, `aria2/src/ui/progress_bar.rs` | Width calculation unit coverage and one Windows terminal resize check | PASS |
| RPC lifecycle as a UI consumer | RPC event callbacks in `src/aria2api.cc` | `aria2-rpc/src/server/ws_session.rs`, `aria2/src/app/rpc_backend.rs` | WebSocket JSON-RPC E2E: start/stop, oversized request, reconnect | PASS |
| Graceful and forced RPC shutdown | `src/aria2api.cc` (`shutdown`) | `aria2/src/app/rpc_backend.rs`, `aria2-core/src/engine/engine_loop.rs` | WebSocket reconnect E2E and engine halt tests; force path is immediate and bounded | PASS |
| Full-screen TUI framework | Original console readout, not a separate widget framework | Text/in-place renderer using `crossterm` width detection | The original has no ratatui-equivalent public contract; adding one would change output semantics | JUSTIFIED_DIFFERENCE |

## Final Regression Closure

The force-shutdown path now sends a synthetic `TaskResult::Cancelled` through
the same completion queue when a running task exceeds the bounded abort wait.
This removes the running generation and decrements `num_commands`, so an
aborted protocol task cannot keep the engine alive indefinitely or block stopped
group cleanup.

An explicit force shutdown is also recorded by the shared request-group
manager. The application preserves any already-recorded stopped error for RPC
inspection, but returns a successful process exit code for the intentional
force-shutdown operation instead of misreporting that shutdown as a failed
download run.

Verified on the `ui` branch:

```text
cargo fmt --all -- --check
git diff --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p aria2-core --lib engine::engine_loop -- --test-threads=1  # 23 passed
cargo test -p aria2 --test e2e_websocket_rpc_client -- --nocapture --test-threads=1  # 3 passed
```

The SFTP default timeout assertions are separately verified: default options
produce no command-level timeout, while an explicit `timeout=300` produces a
300-second timeout. The Debian public torrent remains a protocol fixture only;
its observed zero public upload is not used as a false claim of client upload
failure, and no Debian payload was retained in the workspace.

## Reproducible gates

```text
cargo fmt --all -- --check
git diff --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p aria2 --test test_cli_options
cargo test -p aria2 --test test_config_file
cargo test -p aria2 --test e2e_cli_ui -- --nocapture
cargo test -p aria2 --test e2e_websocket_rpc_client -- --nocapture --test-threads=1
cargo test -p aria2-core --lib engine::engine_loop -- --nocapture --test-threads=1
```

The public swarm Debian torrent is an independent BitTorrent protocol E2E
fixture. A public peer not requesting bytes from this client is not evidence
of a CLI/UI defect; upload acceptance requires a controlled peer that requests
at least one verified piece from this client.

## Final CI and cleanup evidence

GitHub Actions run `32619334218` passed on the `ui` branch at commit
`5735e70a35a2b7514118ace5de888b98d9c0378b`: lint/format, Linux, macOS, and
Windows jobs all succeeded. The controlled upload E2E
`test_bt_download_to_seed_upload_and_ratio_exit_over_tcp` completed a real
TCP peer request and verified the exact piece payload, uploaded byte count,
unchoke, and ratio-based seeding shutdown. After verification,
`cargo clean` removed 10,261 build files and 7.8 GiB; no `target` directory,
Debian ISO, or partial download remains in the workspace. The original
`aria2_original/debian-13.5.0-amd64-DVD-1.iso.torrent` fixture was preserved.
