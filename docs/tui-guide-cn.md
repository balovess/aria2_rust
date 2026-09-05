# aria2_rust TUI 使用指南

English: [`tui-guide-en.md`](tui-guide-en.md)

TUI 是 aria2-rust 提供的交互式终端界面，适合首次使用、查看任务状态以及控制本地或远程 aria2 服务。

TUI 是可选功能。默认构建不包含终端 UI；需要使用 TUI 时请启用 `tui` feature：

```bash
cargo build --release -p aria2 --features tui
```

发行版同时提供 `*-minimal`、`*-standard`、`*-tui` 和 `*-full` 四类二进制包。TUI 用户请选择 `*-tui` 或 `*-full`。

## 启动

本地模式直接启动：

```bash
aria2c tui
```

也可以使用全局选项：

```bash
aria2c --tui --language=zh-CN
```

语言支持 `en-US`、简体中文（`zh-CN`）、繁体中文（`zh-TW`/`zh-HK`）、日语、西班牙语、俄语、印地语、孟加拉语、泰米尔语、越南语、泰语和印尼语；未指定时读取 `LC_ALL` 或 `LANG`，不识别的语言使用英文。南亚和东南亚语言采用区域前缀识别，例如 `hi-IN`、`bn-BD`、`ta-IN`、`vi-VN`、`th-TH` 和 `id-ID`。

## 远程 RPC 模式

先启动 aria2 RPC 服务，并配置 `rpc-secret`：

```bash
aria2c --enable-rpc=true --rpc-listen-port=6800 --rpc-secret=SECRET
```

再启动 TUI 客户端：

```bash
aria2c --rpc-url http://127.0.0.1:6800/jsonrpc --rpc-token SECRET
```

RPC TUI 使用标准 JSON-RPC 2.0。状态查询会将 active、waiting 和 stopped 合并为一个 HTTP 批量请求；活动任务约每 750ms 刷新，空闲时约每 3s 刷新。网络错误会显示在底部并自动重试，连接超时为 3 秒，请求超时为 10 秒。

## 操作

| 按键 | 操作 |
| --- | --- |
| `a` | 添加 URL |
| `/` | 按 URI 或 GID 筛选 |
| `d` | 显示或隐藏任务详情 |
| `p` | 暂停或继续当前任务 |
| `r` | 删除当前任务 |
| `↑` / `↓` | 选择任务 |
| `[` / `]` | 上一页或下一页 |
| `PageUp` / `PageDown` | 快速翻页 |
| `q` / `Esc` | 退出 |

RPC 模式每页读取 100 个 waiting 和 100 个 stopped 历史任务，active 任务会显示在每一页。筛选只作用于当前页；切换页面后会重新从 RPC 服务端读取数据。

## 兼容性和限制

- TUI 不修改 aria2 RPC 方法、认证 token 格式或返回结构。
- RPC 模式需要服务端支持 HTTP JSON-RPC 批量请求；aria2-rust 内置 RPC 服务支持该格式。
- 当前界面默认展示 active、waiting 和 stopped 历史任务。
- RPC 地址不应暴露到公网；应同时使用 token 和防火墙规则。

## 翻译资源维护

翻译资源位于 `aria2/src/ui/resources/`，每种语言使用一个 TOML 文件。新增语言时复制现有文件并填写 `title`、`empty`、`footer`、`add_prompt`、`filter_prompt`、`filtered`、`details`、`headers`、`remote_headers`、`detail_labels`、`statuses`、`page` 和 `error` 字段。

资源在编译后嵌入程序并缓存解析结果。完整性测试会检查 TOML 可解析、字段非空、表头和状态数量正确，以及 `{page}`、`{next}`、`{message}` 占位符可用：

```bash
cargo test -p aria2 --lib resources::tests
```
