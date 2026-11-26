# aria2_rust Release Artifacts

中文：[`release-artifacts-cn.md`](release-artifacts-cn.md)

Release checklist: [`release-checklist-en.md`](release-checklist-en.md)

## Variants

Each platform provides three binary packages:

| Suffix | Contents | Recommended for |
| --- | --- | --- |
| `-minimal` | HTTP/FTP + RPC, without BitTorrent or TUI | Servers, scripts, and low-dependency deployments |
| `-standard` | HTTP/FTP + BitTorrent + RPC, without TUI | General downloads and remote control |
| `-full` | standard + TUI + Metalink + SFTP | Users needing all protocol features |

The `-minimal` build enables HTTP/FTP, keeps the JSON-RPC/XML-RPC/WebSocket server available, and excludes BitTorrent and TUI dependencies. RPC is also available in `-standard`; the `rpc` Cargo feature is a tier marker, not a source-code gate. `-standard` adds BitTorrent. The prebuilt `-full` package adds TUI, Metalink, and SFTP. To build a TUI package without Metalink or SFTP, build from source with `--features tui`.

## Current platforms

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `.tar.gz` |
| Linux ARM64 | `.tar.gz` |
| Windows x86_64 | `.zip` |
| macOS ARM64 | `.tar.gz` |
| macOS x86_64 | `.tar.gz` |

For example, Linux x86_64 provides:

```text
aria2-x86_64-linux-minimal.tar.gz
aria2-x86_64-linux-standard.tar.gz
aria2-x86_64-linux-full.tar.gz
```

Every archive has a matching `.sha256` file. Verify the digest before extracting and running `aria2c` (`aria2c.exe` on Windows).

## CI and release triggers

- Dev CI is manual-only (`workflow_dispatch`); pushes and pull requests do not start it automatically.
- The Release workflow runs on every push to `master` and can also be dispatched manually from `master`. It creates a new binary release only when the version-derived tag does not already exist; manual dispatch can retry package-manager updates for an existing release.
- The Release page automatically includes a three-tier artifact selection table and checksum files for every platform.

## Build from source

```bash
# minimal
cargo build --release -p aria2 --no-default-features --features http

# standard
cargo build --release -p aria2 --no-default-features --features standard

# tui
cargo build --release -p aria2 --no-default-features --features tui

# full
cargo build --release -p aria2 --no-default-features --features full
```

See [`tui-guide-en.md`](tui-guide-en.md) for detailed TUI usage.
