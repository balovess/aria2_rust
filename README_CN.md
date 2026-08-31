# aria2-rust

English: [`README.md`](README.md)

> **版本提示：** aria2-rust 当前处于快速迭代阶段，旧版本可能遗留各类问题，
> 无法保证基本功能的可用性。请及时使用最新版本。

## 文档导览

请先查看[文档索引](docs/README-cn.md)。常用入口：

- [快速开始](docs/quickstart-cn.md)
- [参数配置说明](docs/configuration-guide-cn.md)
- [RPC 使用说明](docs/rpc-guide-cn.md)
- [常见问题](docs/troubleshooting-cn.md)

<p align="center">
  <strong>超高速下载工具 —— Rust 语言重写</strong>
</p>

<p align="center">
  <a href="#特性">特性</a> •
  <a href="#快速开始">快速开始</a> •
  <a href="#使用方法">使用方法</a> •
  <a href="#项目架构">项目架构</a> •
  <a href="#性能">性能</a> •
  <a href="#构建说明">构建说明</a> •
  <a href="#许可证">许可证</a>
</p>

***

**aria2-rust** 是知名下载工具 [aria2](https://aria2.github.io/) 的 Rust
实现，当前仍在以 `aria2_original` 为基准进行兼容迁移。默认构建支持
HTTP/HTTPS、FTP、BitTorrent 协议，并提供
JSON-RPC/XML-RPC/WebSocket 远程控制接口；完成度以
[docs/compatibility-status.md](docs/compatibility-status.md) 为准。
SFTP 和 Metalink 需要分别启用对应 Cargo feature。

## 特性

- **多协议下载**: 默认构建支持 HTTP/HTTPS、FTP、BitTorrent；SFTP 和 Metalink 由 feature 控制
- **多源镜像**: 自动从多个 URI 分段并行下载，最大化带宽利用率
- **断点续传**: HTTP/HTTPS 等主要路径支持控制文件续传；不同协议、并发控制文件和多 URI 失败回退仍按兼容性矩阵逐项验证
- **BitTorrent 完整支持**:
  - ✅ DHT 网络（KRPC + 路由表 + bootstrap 节点）
  - ✅ Tracker 通信（UDP/HTTP）
  - ✅ Peer 交换（PEX，按 peer 进行 BEP 10 扩展 ID 协商）
  - ✅ MSE/PE 加密（BEP14 握手）
  - ✅ 阻塞算法 + seed-time/ratio 支持
  - ✅ RarestFirst Piece 选择
- **速率限制**: 令牌桶算法，支持全局/单任务限速
- **Cookie 管理**: Netscape 格式持久化 + 自动从文件加载
- **会话管理**: 自动保存 + 手动保存/加载，使用 .aria2 控制文件
- **RPC 远程控制**: JSON-RPC 2.0、XML-RPC、WebSocket（按编译 feature 返回原版方法/通知目录：核心 33 个方法和 5 个通知，BitTorrent/Metalink 启用后分别增加对应能力；全 feature 为 36/6）
- **配置系统**: 类型化参数注册表，支持命令行 / 配置文件 / 环境变量 / 默认值四源合并
- **NetRC 认证**: 自动从 `.netrc` 文件读取 FTP/HTTP 凭证
- **URI 列表文件**: 支持 `-i` 参数批量导入下载任务
- **公共 Tracker 列表**: 自动从 trackerslist.com 更新 BT Peer 发现

## 快速开始

### 前置条件

- [Rust](https://www.rust-lang.org/tools/install) 1.70+ (稳定版)
- Windows / macOS / Linux

### 构建和运行

```bash
# 克隆仓库
git clone https://github.com/balovess/aria2_rust.git
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

完整的用户使用说明、常用命令、配置文件、RPC、会话管理以及配置检查/修复/重置流程见
[用户使用指南](docs/user-guide.md)。

### 基础 HTTP 下载

```bash
aria2c http://example.com/file.zip
```

### 使用选项

```bash
aria2c -o output.dat -d /downloads -s 4 -x 8 http://example.com/large.bin
```

| 选项                                | 说明          | 默认值   |
| --------------------------------- | ----------- | ----- |
| `-d, --dir`                       | 保存目录        | `.`   |
| `-o, --out`                       | 输出文件名       | 自动    |
| `-s, --split`                     | 每个下载的并发分片请求数   | `16`   |
| `-x, --max-connection-per-server` | 每个服务器的最大并发分片请求数 | `16`   |
| `--max-download-limit`            | 最大下载速度      | 无限制   |
| `--timeout`                       | 超时时间（秒）     | `60`  |
| `-q, --quiet`                     | 安静模式        | false |

对于支持 Range 的 HTTP 分段下载，`split` 是单个文件的总并发请求预算；
`max-connection-per-server` 是每个 `scheme://host:port` 服务器的独立上限。
多镜像下载时，不同服务器可以在 `split` 总预算内并行；属于同一服务器的
不同镜像 URL 共享该服务器上限。HTTP 自适应并发从配置上限开始，只有返回
`429` 或 `503` 的服务器会降低并发目标，其他镜像不受影响。
### BitTorrent 下载

```bash
aria2c file.torrent
```

### URI 列表文件

创建包含 URI 的文本文件（每个条目占一块，Tab 分隔镜像源）：

```
  dir=/downloads
  split=16
http://mirror1.example.com/file.iso	http://mirror2.example.com/file.iso
http://mirror3.example.com/file.iso
```

然后：

```bash
aria2c -i uris.txt
```

## 项目架构

总代码量：以当前 workspace 源码为准（持续统计中）\
测试套件：按 Cargo 测试目标和 feature 分层统计，未使用单一总数冒充全量覆盖

本项目组织为 Cargo workspace，包含 4 个子项目：

```
aria2-rust/
├── aria2/                  # 二进制子项目（CLI 入口，~550 行）
│   ├── src/main.rs        #   程序入口
│   ├── src/app.rs         #   应用运行时（ConfigManager + Engine）
│   └── examples/          #   使用示例
├── aria2-core/             # 核心库（~7,000 行）
│   ├── src/engine/        #   下载引擎（12 个命令实现）
│   │   ├── process_wait.rs # 原生进程退出事件与兼容 fallback
│   │   ├── download_engine.rs # 带命令队列的事件循环
│   │   ├── download_command.rs # HTTP/HTTPS 下载器
│   │   ├── ftp_download_command.rs # FTP/SFTP 下载器
│   │   ├── bt_download_command.rs # BitTorrent 下载器
│   │   ├── magnet_download_command.rs # Magnet 链接下载器
│   │   ├── metalink_download_command.rs # Metalink 下载器
│   │   └── concurrent_download_command.rs # 多段下载器
│   ├── src/config/        #   类型化参数注册表和解析器
│   │   ├── option.rs     #     OptionType/Value/Def/Registry
│   │   ├── parser.rs     #     多源解析器（CLI/文件/环境变量/默认值）
│   │   ├── netrc.rs      #     NetRC 认证解析器
│   │   ├── uri_list.rs  #     URI 列表文件（-i 选项）解析器
│   │   └── mod.rs        #     ConfigManager 统一运行时管理器
│   ├── src/request/       #   请求管理
│   │   ├── request_group_man.rs # 全局任务管理器
│   │   └── request_group/      # 每个任务的状态机和活动信号
│   │       └── activity.rs     # Notify + generation 唤醒信号
│   ├── src/filesystem/     #   磁盘 I/O
│   │   ├── disk_writer.rs # 磁盘写入接口
│   │   ├── disk_cache.rs # 缓存写入器（256KB 直写）
│   │   ├── control_file.rs # .aria2 控制文件格式
│   │   ├── file_allocation.rs # 预分配策略
│   │   └── checksum.rs # 校验和验证
│   ├── src/http/          #   Cookie 管理
│   │   ├── cookie.rs # Cookie 结构
│   │   ├── cookie_storage.rs # 持久化存储
│   │   └── ns_cookie_parser.rs # Netscape 格式解析器
│   ├── src/session/       #   会话持久化
│   │   ├── session_serializer.rs # 序列化
│   │   ├── auto_save_coordinator.rs # 统一持久化 deadline 调度
│   │   ├── auto_save_session.rs # 自动保存
│   │   └── save_session_command.rs # 退出时保存
│   ├── src/rate_limiter.rs # 令牌桶速率限制
│   └── src/ui.rs           #   进度条和状态面板
├── aria2-protocol/         # 协议栈（~5,000 行）
│   ├── src/http/           #   HTTP/HTTPS 客户端（认证/代理/Cookie/压缩）
│   ├── src/ftp/            #   FTP/SFTP 客户端（匿名 + 认证，被动模式）
│   ├── src/bittorrent/     #   完整 BT 协议栈
│   │   ├── bencode/ # BEP3 bencode 编解码
│   │   ├── torrent/ # .torrent 文件解析
│   │   ├── magnet.rs # Magnet 链接解析
│   │   ├── dht/ # KRPC + 路由表 + bootstrap
│   │   ├── tracker/ # UDP/HTTP tracker
│   │   ├── peer/ # Peer 连接 + 握手
│   │   ├── extension/ # MSE/PEX/ut_metadata
│   │   └── piece/ # Piece 管理器 + 选择器
│   └── src/metalink/      #   Metalink V3/V4 解析
├── aria2-rpc/              # RPC 服务器（~1,000 行）
│   ├── src/json_rpc.rs     #   JSON-RPC 2.0 编解码
│   ├── src/xml_rpc.rs      #   XML-RPC 编解码
│   ├── src/websocket.rs    #   WebSocket 事件发布
│   ├── src/server.rs       #   HTTP 服务器（认证/CORS/状态）
│   └── src/engine.rs       #   RpcEngine 请求分发和协议桥接
└── bindings/               # 语言绑定（~1,200 行）
    ├── python/            #   Python SDK（~600 行）
    └── nodejs/            #   Node.js SDK（~627 行 TS）
└── Cargo.toml              # Workspace 配置
```

## 性能

相对 `aria2_original` 的差异化性能实现统一整理在
[性能差异化台账](docs/performance-differentiators.md)，包括原版基线、Rust 实现、
兼容性边界、代码入口和验证状态。当前主要差异如下：

| 领域 | 当前 Rust 实现 |
| --- | --- |
| 磁盘 I/O | Positioned offset write、写回 range cache、阈值批处理和多文件 coalescing；阻塞 syscall 放入 Tokio blocking pool，Linux `io_uring` 为 opt-in backend。 |
| 数据路径 | 通过 `bytes::Bytes` 在 cache、Piece writer 和多文件切片之间传递，减少复制和临时分配；这是 reduced-copy path，不是端到端 zero-copy 保证。 |
| Hash 校验 | 有界后台 hash worker、分块完整性 dispatcher、协作式让出和 RequestGroup 生命周期取消。 |
| BT/DHT | Hash-based peer 生命周期、增量 Piece 频率、共享 HAVE frame 的有界并发发送、bucket tree/top-K 路由和有界 UDP worker。 |
| 文件预分配 | Linux `fallocate`、Windows `SetFileValidData`、macOS `F_PREALLOCATE` 的平台适配，以及不会阻塞 reactor 的 fallback。 |
| RPC 控制面 | owned wire parsing、HTTP/WebSocket batch 中最多 64 路只读并发、mutation barrier，以及重 payload 转换的 blocking worker；`system.multicall` 保留原版顺序语义。 |

### 当前版本与原版对比

以下数据来自同一台 Windows 11 x64 机器上的 Release 构建和空闲 RPC 进程测试：

| 指标 | aria2 1.37.0 原版 | aria2-rust 当前参考值 |
| --- | ---: | ---: |
| `aria2c.exe` 文件大小 | 约 5.39 MiB | 约 13.6 MiB |
| 空闲 RPC Working Set | 约 12.5 MiB | 约 16.1 MiB |
| 空闲 RPC Private Bytes | 约 3.25 MiB | 约 3.6 MiB |

数据会随编译 feature、Windows 版本、分配器和测量时机变化，不能替代同负载下载基准。当前版本的 Private Bytes 已接近原版，但二进制体积和 Working Set 仍高于原版。

台账同时记录已知边界：DHT 网络维护和主动 peer lookup 已通过统一 task queue 调度，
周期网络任务在对应 lane 忙碌时会合并重复 tick；token 轮换和本地清理保留在 deadline
协调器内；路由表保存使用 blocking worker。Linux 和 macOS 的真实运行时证据仍需补充，
也尚未建立可与 `aria2_original` C++ binary 对比的同负载完整下载基线。

已有事件驱动热路径包括：

- uTP 接收使用 Tokio UDP readiness 和持久分片缓冲，避免固定 `sleep` 后重复 `recv`。
- BitTorrent piece 使用有界 event-driven block pipeline；endgame 并发消费 peer 响应，
  TCP/MSE 取消后保留未完成帧。
- 动态修改限速会立即唤醒阻塞中的 token 获取；PieceStat、missing-piece 和
  rarest-first 查询减少重复线性扫描。
- 引擎空闲路径只等待命令、任务完成、最早维护 deadline 和 shutdown，不再按固定
  idle tick 扫描。

实现入口：[引擎空闲等待](aria2-core/src/engine/engine_loop.rs)、[活动信号](aria2-core/src/request/request_group/activity.rs)、
[保存 deadline](aria2-core/src/session/auto_save_coordinator.rs) 和
[平台进程等待](aria2-core/src/engine/process_wait.rs)。

Windows release 构建中的 Rust-only Criterion 基准（`50,000` pieces，同一
工作树前后中位数）如下：

| 基准 | 优化前 | 优化后 |
| --- | ---: | ---: |
| Bitfield 全 missing 查询 | 63.4 us | 4.3 us |
| Bitfield sparse selection | 194.9 us | 75.0 us |
| Rarest selection | 54.96 us | 2.13 us |

这些是算法级微基准，不代表完整下载吞吐提升，也不是与
`aria2_original` 的对比结果。详细说明、测试证据和边界条件见
[docs/MIGRATION.md](docs/MIGRATION.md) 及
[docs/engine-loop-performance.md](docs/engine-loop-performance.md)。

运行专项基准：

```bash
cargo bench -p aria2-core --bench segment_scan_bench -- --noplot
cargo bench -p aria2-protocol --features bittorrent --bench sequential_picker_bench -- rarest_selection --noplot
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

## 测试

### 运行测试

```bash
# 运行工作区所有测试
cargo test --workspace

# 运行特定 crate 的测试
cargo test -p aria2-core

# 运行测试并显示详细输出
cargo test --workspace -- --nocapture

# 运行特定测试类别
cargo test "test_e2e"      # E2E 测试
cargo test "test_stress"   # 压力测试
cargo test "test_edge"     # 边缘情况测试
cargo test "test_error"    # 错误路径测试
```

### 测试类别

| 类别 | 前缀 | 描述 |
|------|------|------|
| 单元测试 | `test_` | 内联测试，测试单个函数 |
| 集成测试 | `test_` | 模块交互测试 |
| E2E 测试 | `test_e2e_` | 完整工作流程测试 |
| 压力测试 | `test_stress_` | 高负载稳定性测试 |
| 边缘情况测试 | `test_edge_` | 边界条件测试 |
| 错误路径测试 | `test_error_` | 错误处理测试 |

### 覆盖率报告

```bash
# 安装 cargo-tarpaulin (Linux/macOS)
cargo install cargo-tarpaulin

# 生成 HTML 覆盖率报告
cargo tarpaulin --workspace --out Html --output-dir coverage/

# 生成 LCOV 格式用于 CI
cargo tarpaulin --workspace --out Lcov --output-dir coverage/
```

### 运行性能测试

```bash
# 运行所有性能测试
cargo bench --workspace

# 运行特定性能测试
cargo bench --bench config_bench
```

详细测试指南请参阅 [docs/testing-guide.md](docs/testing-guide.md)。

## 与原版 aria2 的兼容性

下表表示代码路径已实现，不等同于完整兼容。模块级 `PARTIAL`、
`UNVERIFIED`、`MISSING` 状态和验收证据以
[兼容性矩阵](docs/compatibility-status.md) 为准。

对外 RPC/JSON-RPC、XML-RPC、WebSocket、认证、参数、错误码、HTTP 状态和
任务生命周期是严格兼容边界，必须与 `aria2_original` 一致，以保证原版
Chrome 插件和其他客户端无需修改。Rust 内部实现可以在这个边界之后使用
更强的类型、所有权和并发模型继续优化；内部改进不能改变外部可观察契约。

| 功能                  | 状态    | 说明                              |
| ------------------- | ----- | ------------------------------- |
| CLI 参数              | ✅ 核心  | 已实现 \~50 个最常用选项                 |
| 配置文件 (`aria2.conf`) | ✅     | 相同语法格式                          |
| 环境变量                | ✅     | `ARIA2_*` 前缀映射                  |
| JSON-RPC API        | PARTIAL | `system.listMethods` 按 feature 返回原版顺序和清单（33/35/36）；RPC E2E 通过，原版客户端矩阵仍在验证 |
| XML-RPC API         | PARTIAL | methodCall/response/fault 支持；与原版客户端的完整互操作仍在验证 |
| WebSocket 通知        | PARTIAL | `system.listNotifications` 按 feature 返回 5/6 个通知；浏览器插件互操作仍在验证 |
| URI 列表文件 (`-i`)     | ✅     | 镜像 + 内联选项                       |
| NetRC 认证            | ✅     | machine/default/macdef 解析       |
| 会话保存/加载             | ✅     | 往返一致                            |
| Metalink V3/V4      | ✅     | 完整解析                            |
| BitTorrent DHT      | ✅     | KRPC + 路由表 + bootstrap          |
| FTP/SFTP            | ✅     | 被动模式 + 认证                       |
| 速率限制                | ✅     | 令牌桶算法                           |
| Cookie 管理           | ✅     | Netscape 格式持久化                  |
| MSE/PE 加密           | ✅ 完整 | BEP14 握手                        |
| Magnet 链接           | ✅ 完整 | ut_metadata 获取                 |
| RarestFirst Piece   | ✅ 完整 | 完整实现                            |
| Endgame 模式         | ✅ 完整 | 最后 piece 优化                     |
| DHT 持久化           | ✅ 完整 | dht.dat 序列化                     |
| uTP 协议             | ✅ 完整 | 原版 aria2 C++ 不支持               |
| Web Seeds           | ✅ 完整 | BEP 19                           |
| LPD                 | ✅ 完整 | 本地 Peer 发现                      |
| 做种模式             | ✅ 完整 | 上传支持                           |

**已知缺口与验证状态**：

- `aria2.forceShutdown`、`system.listMethods` 和 `system.listNotifications` 已实现，并有 handler/集成测试覆盖。
- HTTPS RPC 已有 TLS 配置、服务器实现和专门测试；更广泛的客户端/服务器互操作测试仍在跟踪。
- IPv6 DHT 已有 CLI 和协议层支持；完整网络互操作覆盖仍在跟踪。
- 仍需逐项对照 `aria2_original` 验证更多 CLI/运行时选项行为。
- `aria2-core/src/c_api.rs` 已提供 opaque-handle `extern "C"`/cdylib 迁移接口；它不是原版 C++ STL 类 ABI 的二进制兼容实现。
- Metalink torrent `metaurl` 依赖生命周期、完整性回调路径和部分协议互操作仍未闭环。
- 尚未建立与 aria2 C++ 的可比性能基线；Rust-only 基准不能证明优于原版。
- Windows 上 workspace all-features 聚合测试仍可能在构建阶段超时，不能据此宣称一次性全量通过。

## 许可证

本项目采用 **GPL-2.0-or-later** 许可证，与原版 [aria2](https://github.com/aria2/aria2) 项目保持一致。

Copyright (C) 2024 aria2-rust contributors.

## 致谢

- [aria2](https://aria2.github.io/) — 启发本项目的原始 C++ 下载工具
- [Tokio](https://tokio.rs/) — Rust 异步运行时
- [Reqwest](https://docs.rs/reqwest/) — HTTP 客户端基础
- [Axum](https://docs.rs/axum/) — RPC 服务器的 Web 框架
