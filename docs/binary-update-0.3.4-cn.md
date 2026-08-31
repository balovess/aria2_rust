# aria2-rust 0.3.4 二进制更新说明

本文说明 `aria2-rust 0.3.4` 的二进制发布内容、平台范围、安装升级方式、
校验方法和兼容性边界。本文针对 GitHub Release 中的 `aria2c` 可执行文件，
不等同于 Rust 库 API 或 C ABI 兼容性声明。

## 1. 版本身份

| 项目 | 内容 |
| --- | --- |
| 产品名称 | `aria2-rust` |
| CLI 程序 | `aria2c` |
| 二进制版本 | `0.3.4` |
| Release 标签 | `v0.3.4` |
| 二进制版本来源 | `aria2/Cargo.toml` |
| 发布分支 | `master` |
| 发布方式 | GitHub Actions Release workflow |

`aria2c --version`、启动信息、RPC `aria2.getVersion` 以及 HTTP/BitTorrent
默认身份均使用二进制包 `aria2` 的版本。`aria2-core`、`aria2-protocol` 和
`aria2-rpc` 是独立的库包，它们的包版本不应被当作二进制文件名或 Release
标签。

## 2. 本次二进制更新内容

0.3.4 主要包含以下面向最终用户的变化：

- 改进进程关闭、定时停止和相关依赖清理，减少退出阶段残留任务和资源未释放
  的情况。
- 改进磁盘缓存的淘汰、合并和重叠区间处理，降低重复读写与无效缓存占用。
- 增加磁盘缓存和 writer I/O 统计，便于通过日志和运行状态观察磁盘写入行为。
- 改进下载限速、缓存刷新和可配置 benchmark 参数，使限速和写回行为更稳定。
- 加强 HTTP Range 响应、BitTorrent 消息以及 Bencode 容器元素限制的校验。
- 改进 BitTorrent 关闭、endgame、tracker、PEX、piece 状态和恢复路径的边界
  处理，并补充真实下载回归覆盖。
- 增加或完善配置检查、后台更新检查和 `check-update` 命令相关能力。
- 继续维护 HTTP、BitTorrent、RPC、Metalink 和 SFTP 的 feature 边界；默认二进制
  启用 HTTP、BitTorrent 和 RPC，Metalink/SFTP 由构建 feature 控制。

本次更新还包含测试、文档和内部模块整理。以上内容描述的是本项目 Rust
实现的行为更新，不表示已经完成与 `aria2_original` 的全部语义、协议或 ABI
等价验证。

## 3. 官方二进制产物

Release workflow 在 `master` 推送后构建以下四个平台：

| 平台 | Rust target | Release 文件 | 压缩格式 |
| --- | --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `aria2-x86_64-linux.tar.gz` | tar + gzip |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `aria2-x86_64-windows.zip` | ZIP |
| macOS Apple Silicon | `aarch64-apple-darwin` | `aria2-aarch64-macos.tar.gz` | tar + gzip |
| macOS Intel | `x86_64-apple-darwin` | `aria2-x86_64-macos.tar.gz` | tar + gzip |

每个压缩包当前只包含对应平台的 `aria2c` 可执行文件：Windows 文件名为
`aria2c.exe`，其他平台文件名为 `aria2c`。Release 构建使用优化等级 3、thin
LTO、`panic=abort` 和符号剥离；因此该文件适合直接运行，但不包含调试符号。

Windows ZIP 同时发布 `aria2-x86_64-windows.zip.sha256`。Linux 和 macOS 当前
没有由 Release workflow 自动生成的独立 checksum asset；下载后应使用本地
工具自行计算并保存结果。

## 4. 下载后校验

### Windows PowerShell

```powershell
$file = '.\\aria2-x86_64-windows.zip'
(Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
Get-Content '.\\aria2-x86_64-windows.zip.sha256'
```

比较两个输出中的 64 位十六进制 SHA-256 值。校验文件中的文件名必须与实际
下载文件一致；不一致时不要解压或覆盖现有安装。

### Linux

```bash
sha256sum aria2-x86_64-linux.tar.gz
tar -xzf aria2-x86_64-linux.tar.gz
./aria2c --version
```

### macOS

```bash
shasum -a 256 aria2-aarch64-macos.tar.gz
tar -xzf aria2-aarch64-macos.tar.gz
./aria2c --version
```

如果系统阻止首次运行，请先确认文件来自可信 Release，再按系统安全策略
允许该可执行文件运行。不要把未经校验的第三方重新打包文件当作官方产物。

## 5. 安装和升级

### 直接安装

解压对应平台压缩包，把 `aria2c` 或 `aria2c.exe` 放入 PATH 中的目录，或在
固定目录中通过绝对路径调用。升级前建议停止旧进程，再替换可执行文件；下载
目录、session 文件、日志文件和配置文件可以继续保留。

```bash
aria2c --version
aria2c --help
```

### Cargo 源码构建

```bash
cargo build -p aria2 --release
```

产物位于 `target/release/aria2c`，Windows 为
`target/release/aria2c.exe`。默认 feature 包含 HTTP、BitTorrent 和 RPC；需要
Metalink 或 SFTP 时，在构建时显式启用：

```bash
cargo build -p aria2 --release --features "metalink,sftp"
```

源码构建不等同于官方 Release 二进制：编译器版本、feature、链接器、系统库和
构建参数都可能不同。

### Scoop

Windows 可使用仓库维护的 manifest：

```powershell
scoop install https://raw.githubusercontent.com/balovess/aria2_rust/master/scoop/aria2-rust.json
scoop update aria2-rust
```

Scoop manifest 依赖 GitHub Release 中的 Windows ZIP 和 SHA-256 asset。若 Release
尚未发布或 checksum asset 缺失，manifest 更新不会被视为成功。

## 6. 升级注意事项

- 升级前保留配置文件、`.aria2` 控制文件、session 文件和下载目录备份。
- 不要在旧进程仍写入 session 或目标文件时直接覆盖并启动新进程。
- 继续使用已有 RPC 客户端时，应先运行 `aria2c --version`，再检查
  `aria2.getVersion` 和客户端所依赖的 method/option。
- 本版本的 A2CF BitTorrent checkpoint 是 Rust-owned 恢复格式；它不是对
  `aria2_original` 二进制 `.aria2` 格式的完整兼容承诺。
- C API 提供 Rust-owned 的 opaque-handle 接口，但不声明与原 C++ 类、STL 或
  原始 ABI 的二进制兼容。
- Windows、Linux 和 macOS 的平台行为仍受文件系统、权限、TLS 后端和系统安全
  策略影响；官方平台矩阵通过不代表所有第三方文件系统组合均已验证。

## 7. 发布验证边界

发布前至少应确认：

1. `dev` 分支 CI 的 lint/format 和 Linux、Windows、macOS 测试均通过。
2. `master` 上的 Release workflow 使用 `aria2/Cargo.toml` 读取版本，并生成
   四个平台产物。
3. Windows ZIP、checksum asset 和 Release 标签版本一致。
4. 下载后执行 `aria2c --version`，确认输出为 `0.3.4`。
5. 需要 Scoop 时，确认 manifest 中的 URL 和 SHA-256 与 GitHub Release 一致。

CI 通过只证明仓库定义的自动化检查通过；它不自动证明所有发行版包管理器、
第三方 RPC 客户端、原始 aria2 二进制互操作性或跨平台 ABI 兼容性。
