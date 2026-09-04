# aria2_rust Release Artifacts

中文：[`release-artifacts-cn.md`](release-artifacts-cn.md)

## Variants

Each platform provides four binary packages:

| Suffix | Contents | Recommended for |
| --- | --- | --- |
| `-minimal` | Base build without TUI | Servers, scripts, and low-dependency deployments |
| `-standard` | HTTP/FTP + BitTorrent + RPC, without TUI | General downloads and remote control |
| `-tui` | Local TUI, RPC TUI, and localized resources | Interactive terminal users |
| `-full` | standard + Metalink + SFTP + TUI | Users needing all protocol features |

The `-minimal` build enables only HTTP/FTP and excludes BitTorrent, TUI, and TUI dependencies. `-standard` is the default feature tier; `-tui` adds TUI to standard; `-full` additionally enables Metalink and SFTP.

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
aria2-x86_64-linux-tui.tar.gz
aria2-x86_64-linux-full.tar.gz
```

Every archive has a matching `.sha256` file. Verify the digest before extracting and running `aria2c` (`aria2c.exe` on Windows).

## CI and release triggers

- Dev CI is manual-only (`workflow_dispatch`); pushes and pull requests do not start it automatically.
- Release runs automatically only after a pull request is merged into `master`; a direct push to `master` does not publish binaries.
- The Release page automatically includes a four-tier artifact selection table and checksum files for every platform.

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
