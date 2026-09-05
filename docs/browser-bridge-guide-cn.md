# 浏览器会话桥接开发指南

本文面向希望基于 `aria2-core` 开发网盘、防盗链或浏览器会话下载集成的开发者。

## 能力边界

`aria2-core` 提供线程安全的浏览器会话上下文 API，用于运行时更新 Cookie、User-Agent 和签名 Header。HTTP HEAD、Range、重定向和认证重试会在发送前读取最新快照，因此 Token 轮换不需要重建下载任务。

项目不内置 Chrome/CDP 客户端或浏览器扩展；开发者可以用任意授权方式获取浏览器数据，再通过现有 RPC 接口发布快照。

## 全局更新

适合宿主程序嵌入 `aria2-core`，或已有 IPC/HTTP 服务接收浏览器扩展事件：

```rust,no_run
use aria2_core::http::update_global_json;

fn on_browser_event(json: &str) -> Result<(), serde_json::Error> {
    update_global_json(json)
}
```

JSON 格式：

```json
{
  "cookie": "sid=abc; csrf=token",
  "user_agent": "Mozilla/5.0 ...",
  "headers": [
    ["X-Signature", "signed-value"],
    ["Authorization", "Bearer temporary-token"]
  ]
}
```

每次更新都会替换完整快照。桥接器应发送当前仍然有效的全部值，而不是只发送变化字段。清除会话：

```rust,no_run
aria2_core::http::global_browser_context().clear();
```

## 独立上下文

多账号或多站点场景不应共享全局上下文。可以创建独立上下文并绑定到自定义请求策略：

```rust,no_run
use aria2_core::http::{BrowserContext, BrowserContextUpdate, HttpRequestPolicy};

let context = BrowserContext::new();
context.replace(
    BrowserContextUpdate::new()
        .with_cookie("sid=abc")
        .with_user_agent("Mozilla/5.0 ...")
        .with_header("X-Signature", "signed-value"),
);
let policy = HttpRequestPolicy::default().with_browser_context(context.clone());
```

任务显式配置的同名 Header 优先级更高，不会被浏览器快照覆盖。

## 推荐架构

```text
浏览器扩展 / CDP 客户端
          |
          v
开发者实现的桥接器（WebSocket、Named Pipe、本地 HTTP 等）
          |
          v
BrowserContext::replace_json / update_global_json
          |
          v
aria2-core HTTP 请求策略
```

桥接器负责浏览器权限、来源校验、Token 过期判断和跨进程认证；`aria2-core` 负责保存快照并将其应用到 HTTP 请求。

## 通过 aria2c RPC 更新

独立运行的 `aria2c` 启动 RPC 后，使用 JSON-RPC、XML-RPC 或 WebSocket 发送：

```powershell
$body = '{"jsonrpc":"2.0","id":1,"method":"aria2.updateBrowserContext","params":["token:replace-with-rpc-secret",{"cookie":"sid=abc","user_agent":"Mozilla/5.0","headers":[["X-Signature","signed-value"]]}]}'
Invoke-RestMethod -Uri http://127.0.0.1:6800/jsonrpc -Method Post -ContentType 'application/json' -Body $body
```

`aria2.clearBrowserContext` 用于清除当前会话。两个方法都需要 RPC 认证，不会通过 `getGlobalOption` 返回凭证。

WebSocket 客户端使用现有 RPC 地址 `/jsonrpc`，消息仍是标准 JSON-RPC：

```javascript
const socket = new WebSocket("ws://127.0.0.1:6800/jsonrpc");
socket.onopen = () => socket.send(JSON.stringify({
  jsonrpc: "2.0",
  id: 1,
  method: "aria2.updateBrowserContext",
  params: ["token:replace-with-rpc-secret", {
    cookie: "sid=abc",
    user_agent: "Mozilla/5.0",
    headers: [["X-Signature", "signed-value"]]
  }]
}));
```

浏览器扩展或 CDP 客户端可以由开发者实现为 RPC 客户端；RPC 服务本身已经同时覆盖 HTTP JSON-RPC、XML-RPC 和 WebSocket 传输。生产集成仍应增加来源校验、Token 轮换和域名隔离。

## 安全要求

- 只监听本机地址，或使用强随机认证 Token。
- 不要记录 Cookie、Authorization 或签名值。
- 浏览器退出、账号切换或权限撤销时调用 `clear()`。
- 发布完整快照，避免旧 Token 与新 Cookie 组合发送。
- 全局上下文影响所有 HTTP 下载；多账号场景使用独立上下文。

## 当前未提供

- Chrome DevTools Protocol WebSocket 客户端
- 浏览器扩展源码
- 独立 `aria2c` 进程的 Cookie/Token 更新 RPC 方法以外的专用 CDP 协议
- 按域名自动隔离的全局上下文

外部开发者可以基于本 API 实现上述传输层。内置传输适配器还应增加本机授权、域名隔离和凭证脱敏测试。
