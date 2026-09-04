# aria2_rust Release 产物说明

English: [`release-artifacts-en.md`](release-artifacts-en.md)

## 版本类型

每个平台发布四种二进制包：

| 后缀 | 内容 | 适合用户 |
| --- | --- | --- |
| `-minimal` | 不含 TUI 的基础构建 | 服务器、脚本、低依赖部署 |
| `-standard` | HTTP/FTP + BitTorrent + RPC，不含 TUI | 常规下载和远程控制 |
| `-tui` | 包含本地 TUI、RPC TUI 和多语言资源 | 交互式终端用户 |
| `-full` | standard + Metalink + SFTP + TUI | 需要完整协议支持的用户 |

`-minimal` 构建只启用 HTTP/FTP，不包含 BitTorrent、TUI 及 TUI 的额外依赖。`-standard` 是默认功能档位；`-tui` 在 standard 基础上增加 TUI；`-full` 再增加 Metalink 和 SFTP。

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
aria2-x86_64-linux-tui.tar.gz
aria2-x86_64-linux-full.tar.gz
```

每个压缩包都有对应的 `.sha256` 文件。下载后应先校验摘要，再解压并运行其中的 `aria2c`（Windows 为 `aria2c.exe`）。

## CI 与发布触发规则

- Dev CI 只支持手动触发（GitHub Actions 的 `workflow_dispatch`），不会因提交或创建 Pull Request 自动运行。
- Release 只在分支 Pull Request 合并到 `master` 后自动运行；直接推送到 `master` 不会触发二进制发布。
- Release 页面会自动生成四档产物选择表，并附带每个平台的校验文件。

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
