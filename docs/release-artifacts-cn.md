# aria2_rust Release 产物说明

English: [`release-artifacts-en.md`](release-artifacts-en.md)

发布检查清单：[`release-checklist-cn.md`](release-checklist-cn.md)

## 版本类型

每个平台发布三种二进制包：

| 后缀 | 内容 | 适合用户 |
| --- | --- | --- |
| `-minimal` | HTTP/FTP + RPC，不含 BitTorrent 和 TUI | 服务器、脚本、低依赖部署 |
| `-standard` | HTTP/FTP + BitTorrent + RPC，不含 TUI | 常规下载和远程控制 |
| `-full` | standard + TUI + Metalink + SFTP | 需要完整协议支持的用户 |

`-minimal` 构建启用 HTTP/FTP，同时保留 JSON-RPC/XML-RPC/WebSocket 服务；它不包含 BitTorrent 和 TUI 依赖。RPC 在 `-standard` 中也可用；Cargo 的 `rpc` feature 是档位标记，不是源码裁剪开关。`-standard` 增加 BitTorrent。预编译的 `-full` 包增加 TUI、Metalink 和 SFTP。如需不带 Metalink/SFTP 的 TUI 版本，可从源码用 `--features tui` 构建。

## 当前平台

| 平台 | 压缩格式 |
| --- | --- |
| Linux x86_64 | `.tar.gz` |
| Linux ARM64 | `.tar.gz` |
| Windows x86_64 | `.zip` |
| macOS ARM64 | `.tar.gz` |
| macOS x86_64 | `.tar.gz` |

例如 Linux x86_64 的产物为：

```text
aria2-x86_64-linux-minimal.tar.gz
aria2-x86_64-linux-standard.tar.gz
aria2-x86_64-linux-full.tar.gz
```

每个压缩包都有对应的 `.sha256` 文件。下载后应先校验摘要，再解压并运行其中的 `aria2c`（Windows 为 `aria2c.exe`）。

## CI 与发布触发规则

- Dev CI 只支持手动触发（GitHub Actions 的 `workflow_dispatch`），不会因提交或创建 Pull Request 自动运行。
- Release 工作流在每次推送到 `master` 时运行，也可从 `master` 手动触发。只有版本号生成的 tag 尚不存在时才创建新二进制 Release；已有 Release 可通过手动触发重试包管理器更新。
- Release 页面会自动生成三档产物选择表，并附带每个平台的校验文件。

## 从源码构建

```bash
# minimal
cargo build --release -p aria2 --no-default-features --features http

# standard
cargo build --release -p aria2 --no-default-features --features standard

# 包含 TUI
# tui
cargo build --release -p aria2 --no-default-features --features tui

# full
cargo build --release -p aria2 --no-default-features --features full
```

TUI 的详细用法见 [`tui-guide-cn.md`](tui-guide-cn.md)。
