# aria2_rust TUI Guide

中文：[`tui-guide-cn.md`](tui-guide-cn.md)

The TUI is an interactive terminal interface for starting downloads, monitoring tasks, and controlling a local or remote aria2 service.

The TUI is optional. Default builds do not include the terminal UI; enable the `tui` feature when it is needed:

```bash
cargo build --release -p aria2 --features tui
```

Binary releases provide `*-minimal`, `*-standard`, `*-tui`, and `*-full` packages. TUI users should choose `*-tui` or `*-full`.

## Start

Start the local mode with:

```bash
aria2c tui
```

Or use the global options:

```bash
aria2c --tui --language=en-US
```

Supported locales include English, Simplified Chinese (`zh-CN`), Traditional Chinese (`zh-TW`/`zh-HK`), Japanese, Spanish, Russian, Hindi, Bengali, Tamil, Vietnamese, Thai, and Indonesian. When no language is specified, the TUI checks `LC_ALL` and `LANG`, then falls back to English. South and Southeast Asian locales are selected by regional prefixes such as `hi-IN`, `bn-BD`, `ta-IN`, `vi-VN`, `th-TH`, and `id-ID`.

## Remote RPC mode

Start aria2 with RPC and a secret:

```bash
aria2c --enable-rpc=true --rpc-listen-port=6800 --rpc-secret=SECRET
```

Then start the TUI client:

```bash
aria2c --rpc-url http://127.0.0.1:6800/jsonrpc --rpc-token SECRET
```

The RPC TUI uses standard JSON-RPC 2.0. Active, waiting, and stopped queries are sent in one HTTP batch request. Refreshing is approximately every 750ms while a task is active and every 3s while idle. Network errors remain visible in the footer and trigger automatic retries. Connection timeout is 3 seconds and request timeout is 10 seconds.

## Controls

| Key | Action |
| --- | --- |
| `a` | Add a URL |
| `/` | Filter by URI or GID |
| `d` | Toggle task details |
| `p` | Pause or resume the selected task |
| `r` | Remove the selected task |
| `Up` / `Down` | Select a task |
| `[` / `]` | Previous or next page |
| `PageUp` / `PageDown` | Jump between pages |
| `q` / `Esc` | Quit |

RPC mode reads 100 waiting and 100 stopped tasks per page; active tasks appear on every page. Filtering applies to the current page; changing pages fetches the corresponding ranges from the RPC server.

## Compatibility and limits

- The TUI does not change aria2 RPC method names, token format, or response structures.
- RPC mode requires HTTP JSON-RPC batch support; the built-in aria2-rust RPC server supports it.
- The default view includes active, waiting, and stopped history.
- Do not expose the RPC endpoint publicly; use a token and firewall restrictions.

## Translation resources

Translation resources live in `aria2/src/ui/resources/`, with one TOML file per locale. To add a locale, copy an existing file and fill `title`, `empty`, `footer`, `add_prompt`, `filter_prompt`, `filtered`, `details`, `headers`, `remote_headers`, `detail_labels`, `statuses`, `page`, and `error`.

Resources are embedded into the binary and parsed once, then cached. The completeness test checks TOML parsing, non-empty fields, required array sizes, and the `{page}`, `{next}`, and `{message}` placeholders:

```bash
cargo test -p aria2 --lib resources::tests
```
