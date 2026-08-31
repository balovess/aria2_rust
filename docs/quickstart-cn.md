# aria2-rust 快速开始

## 1. 获取程序

仓库当前提供 Rust 源码构建方式。需要 Rust stable 和 Cargo：

```text
cargo build -p aria2 --release
```

生成的程序位于 `target/release/aria2c`（Windows 为 `target/release/aria2c.exe`）。默认构建包含 HTTP、BitTorrent 和 RPC；需要 Metalink 或 SFTP 时启用对应 feature：

```text
cargo build -p aria2 --release --features "metalink,sftp"
```

## 2. 第一个下载

```text
aria2c https://example.com/file.zip
aria2c --dir=downloads --out=file.zip https://example.com/file.zip
aria2c --continue=true https://example.com/file.zip
```

BitTorrent 支持 `.torrent` 文件和 Magnet URI。Metalink 只有在启用 `metalink` feature 的构建中可用。

## 3. 使用配置文件

```text
aria2c --conf-path=aria2.conf https://example.com/file.zip
aria2c --conf-path=aria2.conf --check-config
```

最小配置：

```ini
dir=downloads
continue=true
split=4
max-connection-per-server=4
```

完整参数请看 [`configuration-guide-cn.md`](configuration-guide-cn.md)，现成模板位于 [`../examples/configs/`](../examples/configs/)。

## 4. 共享配置（推荐）

推荐让后台 RPC 服务和偶尔的命令行下载复用同一份配置：

```ini
dir=downloads
continue=true
enable-rpc=true
rpc-listen-address=127.0.0.1
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
```

```text
# 启动后台 RPC 服务
aria2c --conf-path=aria2.conf --daemon=true

# 普通下载不会因配置中的 enable-rpc 再次监听 6800
aria2c --conf-path=aria2.conf https://example.com/file.zip
```

共享配置不要写 `daemon=true`，避免普通命令行下载被后台化。需要当前命令同时下载并接受 RPC 时，显式添加 `--enable-rpc=true`；这属于与原版不同的显式 RPC 模式，不能依赖原版在 Windows 上的 `SO_REUSEADDR` 端口复用行为。

## 5. 独立 RPC 服务

上面的共享配置在没有下载输入时就是 RPC-only 服务配置。使用不带 URI、torrent、Metalink 或 session 输入的命令启动：

```text
aria2c --conf-path=aria2.conf --daemon=true
```

RPC 地址默认为 `http://127.0.0.1:6800/jsonrpc`。完整方法、认证和 HTTPS 说明请看 [`rpc-guide-cn.md`](rpc-guide-cn.md)。

## 6. 常用下一步

- 断点续传：设置 `continue=true`，保留下载目录中的 `.aria2` 控制文件。
- 会话恢复：同时设置 `input-file=aria2.session` 和 `save-session=aria2.session`。
- 检查配置：使用 `--check-config`，不要直接启动后再猜测参数是否生效。
- 查看全部参数：运行 `aria2c --help`。
