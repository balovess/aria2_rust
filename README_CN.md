# aria2-rust

<p align="center">
  <strong>超高速下载工具 —— Rust 语言重写</strong>
</p>

<p align="center">
  <a href="#特性">特性</a> •
  <a href="#快速开始">快速开始</a> •
  <a href="#使用方法">使用方法</a> •
  <a href="#项目架构">项目架构</a> •
  <a href="#构建说明">构建说明</a> •
  <a href="#许可证">许可证</a>
</p>

---

**aria2-rust** 是知名下载工具 [aria2](https://aria2.github.io/) 的完整 Rust 重写版本。支持 HTTP/HTTPS、FTP/SFTP、BitTorrent、Metalink 协议，提供 JSON-RPC/XML-RPC/WebSocket 远程控制接口。

## 特性

- **多协议下载**: HTTP/HTTPS、FTP/SFTP、BitTorrent (DHT/PEX/MSE)、Metalink V3/V4
- **多源镜像**: 自动从多个 URI 分段并行下载，最大化带宽利用率
- **断点续传**: 支持所有协议的断点续传，网络中断后无缝恢复
- **BitTorrent 完整支持**: DHT 网络、Tracker 通信、Peer 交换 (PEX)、MSE 加密、阻塞算法
- **RPC 远程控制**: JSON-RPC 2.0、XML-RPC、WebSocket 实时事件推送
- **配置系统**: ~95 个核心选项，支持命令行 / 配置文件 / 环境变量四源合并
- **NetRC 认证**: 自动从 `.netrc` 文件读取 FTP/HTTP 凭证
- **URI 列表文件**: 支持 `-i` 参数批量导入下载任务

## 快速开始

### 前置条件

- [Rust](https://www.rust-lang.org/tools/install) 1.70+ (稳定版)
- Windows / macOS / Linux

### 构建和运行

```bash
# 克隆仓库
git clone https://github.com/aria2/aria2-rust.git
cd aria2-rust

# 构建所有子项目
cargo build --release

# 下载文件 (HTTP)
cargo run --release -- http://example.com/file.zip

# 使用自定义选项下载
cargo run --release -- -d ./downloads -s 4 http://example.com/large.iso

# 显示帮助
cargo run --release -- --help

# 显示版本
cargo run --release -- --version
```

## 使用方法

### 基础 HTTP 下载

```bash
aria2c http://example.com/file.zip
```

### 使用选项

```bash
aria2c -o output.dat -d /downloads -s 4 -x 8 http://example.com/large.bin
```

| 选项 | 说明 | 默认值 |
|--------|-------------|---------|
| `-d, --dir` | 保存目录 | `.` |
| `-o, --out` | 输出文件名 | 自动 |
| `-s, --split` | 每个服务器的连接数 | `1` |
| `-x, --max-connection-per-server` | 每个服务器的最大连接数 | `1` |
| `--max-download-limit` | 最大下载速度 | 无限制 |
| `--timeout` | 超时时间（秒） | `60` |
| `-q, --quiet` | 安静模式 | false |

### BitTorrent 下载

```bash
aria2c file.torrent
```

### URI 列表文件

创建包含 URI 的文本文件（每个条目占一块，Tab 分隔镜像源）：

```
  dir=/downloads
  split=5
http://mirror1.example.com/file.iso	http://mirror2.example.com/file.iso
http://mirror3.example.com/file.iso
```

然后：

```bash
aria2c -i uris.txt
```

## 项目架构

本项目组织为 Cargo workspace，包含 4 个子项目：

```
aria2-rust/
├── aria2/                  # 二进制子项目（CLI 入口）
│   ├── src/main.rs        #   程序入口
│   ├── src/app.rs         #   应用运行时（ConfigManager + Engine）
│   └── examples/          #   使用示例
├── aria2-core/             # 核心库（引擎 + 配置 + UI）
│   ├── src/config/        #   选项注册表、解析器、ConfigManager
│   │   ├── option.rs     #     OptionType/Value/Def/Registry（~95 个选项）
│   │   ├── parser.rs     #     多源解析器（CLI/文件/环境变量/默认值）
│   │   ├── netrc.rs      #     NetRC 认证解析器
│   │   ├── uri_list.rs  #     URI 列表文件（-i 选项）解析器
│   │   └── mod.rs        #     ConfigManager 统一运行时管理器
│   ├── src/engine/        #   下载引擎
│   │   └── download_engine.rs # 带命令队列的事件循环
│   ├── src/request/       #   请求管理
│   │   ├── request_group_man.rs # 全局任务管理器
│   │   └── request_group.rs    # 每个任务的状态机
│   ├── src/filesystem/     #   磁盘 I/O（adaptor/writer/cache/allocation）
│   └── src/ui.rs           #   进度条和状态面板
├── aria2-protocol/         # 协议库
│   ├── src/http/           #   HTTP/HTTPS 客户端（认证/代理/Cookie/压缩）
│   ├── src/ftp/            #   FTP/SFTP 客户端（匿名 + 认证，被动模式）
│   └── src/bittorrent/     #   BT 协议（bencode/torrent/DHT/tracker/peer/PEx/MSE）
├── aria2-rpc/              # RPC 库
│   ├── src/json_rpc.rs     #   JSON-RPC 2.0 编解码
│   ├── src/xml_rpc.rs      #   XML-RPC 编解码
│   ├── src/websocket.rs    #   WebSocket 事件发布
│   ├── src/server.rs       #   HTTP 服务器框架（认证/CORS/状态模型）
│   └── src/engine.rs       #   RpcEngine 桥接（25 个 RPC 方法）
└── Cargo.toml              # Workspace 配置
```

## 库使用

### 在 Rust 项目中作为库使用

添加到 `Cargo.toml`：

```toml
[dependencies]
aria2-core = { path = "../aria2-core" }
aria2-rpc = { path = "../aria2-rpc" }
```

#### 最小下载示例

```rust
use aria2_core::config::ConfigManager;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::config::OptionValue;

#[tokio::main]
async fn main() {
    let mut config = ConfigManager::new();
    config.set_global_option("dir", OptionValue::Str("./downloads".into())).await.unwrap();
    config.set_global_option("split", OptionValue::Int(4)).await.unwrap();

    let man = RequestGroupMan::new();
    let opts = DownloadOptions {
        split: Some(4),
        ..Default::default()
    };

    match man.add_group(vec!["http://example.com/file.zip".into()], opts).await {
        Ok(gid) => println!("下载已开始：#{}", gid.value()),
        Err(e) => eprintln!("错误：{}", e),
    }
}
```

#### RPC 服务器示例

```rust
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;

#[tokio::main]
async fn main() {
    let engine = RpcEngine::new();

    let req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.addUri".into(),
        params: serde_json::json!([["http://example.com/file.zip"]]),
        id: Some(serde_json::Value::String("req-1".into())),
    };

    let resp = engine.handle_request(&req).await;
    println!("{}", serde_json::to_string_pretty(&resp).unwrap());
}
```

## 构建说明

### 系统要求

- **Rust**: 1.70 或更高版本（[安装指南](https://rustup.rs/)）
- **操作系统**: Windows 10+, macOS 10.15+, Linux (glibc 2.17+)

### 构建命令

```bash
# 调试构建（快速编译）
cargo build

# 发布构建（优化）
cargo build --release

# 运行测试
cargo test --workspace

# 生成文档
cargo doc --workspace --no-deps

# 运行特定示例
cargo run --example simple_download -- http://example.com/test.bin
```

## 与原版 aria2 的兼容性

| 功能 | 状态 | 说明 |
|---------|--------|-------|
| CLI 参数 | ✅ 核心 | 已实现 ~50 个最常用选项 |
| 配置文件 (`aria2.conf`) | ✅ | 相同语法格式 |
| 环境变量 | ✅ | `ARIA2_*` 前缀映射 |
| JSON-RPC API | ✅ | 25 个方法兼容 |
| XML-RPC API | ✅ | 完整 methodCall/response/fault 支持 |
| WebSocket 事件 | ✅ | 7 种事件类型 |
| URI 列表文件 (`-i`) | ✅ | 镜像 + 内联选项 |
| NetRC 认证 | ✅ | machine/default/macdef 解析 |
| 会话保存/加载 | ✅ | 往返一致 |
| Metalink V3/V4 | ✅ | 完整解析 |
| BitTorrent DHT | ✅ | 引导节点 + KRPC |
| FTP/SFTP | ✅ | 被动模式 + 认证 |

**尚未实现**（计划中）：
- Magnet 链接支持
- Cookie 导入/导出（Firefox/Chrome 格式）
- 实时速度图表（TUI）
- 完整 300+ 选项覆盖（目前 ~95 个核心选项）

## 许可证

本项目采用 **GPL-2.0-or-later** 许可证，与原版 [aria2](https://github.com/aria2/aria2) 项目保持一致。

Copyright (C) 2024 aria2-rust contributors.

## 致谢

- [aria2](https://aria2.github.io/) — 启发本项目的原始 C++ 下载工具
- [Tokio](https://tokio.rs/) — Rust 异步运行时
- [Reqwest](https://docs.rs/reqwest/) — HTTP 客户端基础
- [Axum](https://docs.rs/axum/) — RPC 服务器的 Web 框架
