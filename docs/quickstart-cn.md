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

## 4. 启用 RPC

```ini
enable-rpc=true
rpc-listen-address=127.0.0.1
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
```

```text
aria2c --conf-path=aria2.conf
```

RPC 地址默认为 `http://127.0.0.1:6800/jsonrpc`。完整方法、认证和 HTTPS 说明请看 [`rpc-guide-cn.md`](rpc-guide-cn.md)。

## 5. 常用下一步

- 断点续传：设置 `continue=true`，保留下载目录中的 `.aria2` 控制文件。
- 会话恢复：同时设置 `input-file=aria2.session` 和 `save-session=aria2.session`。
- 检查配置：使用 `--check-config`，不要直接启动后再猜测参数是否生效。
- 查看全部参数：运行 `aria2c --help`。
