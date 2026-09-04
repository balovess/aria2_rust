# aria2-rust TUI Guide

中文：[`tui-guide-cn.md`](tui-guide-cn.md)

The TUI is an interactive terminal interface for starting downloads, monitoring tasks, and controlling a local or remote aria2 service.

## Start

Start the local mode with:

```bash
aria2c tui
```

Or use the global options:

```bash
aria2c --tui --language=en-US
```

Supported locales are `en-US`, `zh-CN`, `ja-JP`, and `es-ES`. When no language is specified, the TUI checks `LC_ALL` and `LANG`, then falls back to English.

## Remote RPC mode

Start aria2 with RPC and a secret:

```bash
aria2c --enable-rpc=true --rpc-listen-port=6800 --rpc-secret=SECRET
```

Then start the TUI client:

```bash
aria2c --rpc-url http://127.0.0.1:6800/jsonrpc --rpc-token SECRET
```

The RPC TUI uses standard JSON-RPC 2.0. Active and waiting queries are sent in one HTTP batch request. Refreshing is approximately every 750ms while a task is active and every 3s while idle. Network errors remain visible in the footer and trigger automatic retries.

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

RPC mode reads 100 waiting tasks per page. Filtering applies to the current page; changing pages fetches the corresponding range from the RPC server.

## Compatibility and limits

- The TUI does not change aria2 RPC method names, token format, or response structures.
- RPC mode requires HTTP JSON-RPC batch support; the built-in aria2-rust RPC server supports it.
- The default view includes active and waiting tasks, but not stopped history.
- Do not expose the RPC endpoint publicly; use a token and firewall restrictions.
