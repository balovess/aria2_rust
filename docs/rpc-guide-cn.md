# aria2-rust RPC 使用说明

English version: [`rpc-guide-en.md`](rpc-guide-en.md)

本文是独立的 RPC 参考。RPC 服务由 `aria2c` 进程启动，默认关闭；启用后同时提供 JSON-RPC 2.0、XML-RPC 和 WebSocket。实际可用的方法以 `system.listMethods` 返回值为准，因为 BitTorrent、Metalink 和 SFTP 能力由构建 feature 决定。

## 1. 启动服务

最小本机配置：

```ini
enable-rpc=true
rpc-listen-address=127.0.0.1
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
```

```text
aria2c --conf-path=aria2.conf
```

默认地址为 `127.0.0.1:6800`。HTTP JSON-RPC 地址是 `http://127.0.0.1:6800/jsonrpc`，XML-RPC 地址是 `http://127.0.0.1:6800/rpc`，WebSocket 连接地址是 `ws://127.0.0.1:6800/jsonrpc`。

### 启动模式差异

aria2-rust 将 RPC 监听作为启动计划的一部分，而不是让配置文件中的 `enable-rpc=true` 无条件影响每次命令行调用：

- 没有下载输入时，`enable-rpc=true` 启动 RPC-only 服务并保持进程运行。
- 有 URI、URI 列表、torrent、Metalink 或 session 恢复任务时，配置文件/环境变量中的 `enable-rpc=true` 不会启动本次 RPC listener；下载完成后进程退出。
- 需要同时下载和接受远程任务时，必须在命令行显式指定 `--enable-rpc=true`。
- `daemon=true` 只改变进程是否后台运行，不改变上述模式选择。

这与 C++ 原版 aria2 按最终 `enable-rpc` 创建 listener 的行为不同，是 aria2-rust 为共享配置场景采用的有意产品差异。它解决了“后台服务和一次性命令共用配置”时的端口占用问题，但也意味着不能仅通过配置文件让带有初始下载输入的命令自动进入下载 + RPC 模式。

原版 C++ aria2 的 listener 设置了 `SO_REUSEADDR`。在 Windows 等系统上，同一地址和端口可能因此被多个进程同时监听；原版还可能通过 IPv4/IPv6 回退掩盖其中一个地址族的占用。这种端口复用不代表多个进程共享任务或 RPC 状态，也不保证请求会到达期望的进程。aria2-rust 的显式 RPC 模式不采用这种跨进程复用：会根据 `disable-ipv6` 尝试地址族，全部绑定失败时直接报告启动错误。需要稳定的单一 RPC 服务时，推荐只运行一个无初始下载输入的 RPC-only 后台进程。

需要远程访问时设置 `rpc-listen-all=true`，或设置明确的 `rpc-listen-address`，并同时使用 `rpc-secret` 和防火墙限制。不要在公网暴露无认证的 RPC。

## 2. 认证

推荐使用 `rpc-secret`。认证 token 作为每个请求 `params` 的第一个元素，服务端会移除它后再按方法解析参数：

```json
{"jsonrpc":"2.0","id":1,"method":"aria2.getVersion","params":["token:replace-with-a-long-random-token"]}
```

未配置 secret 时可以省略 token。`rpc-user`/`rpc-passwd` 是兼容性的 Basic Auth 配置，已标记为 deprecated；客户端也可以发送 `Authorization: Basic ...`。secret 不会由 `getGlobalOption` 返回。

## 3. JSON-RPC

### 单请求

```powershell
$body = '{"jsonrpc":"2.0","id":1,"method":"aria2.addUri","params":[["https://example.com/file.zip"],{"dir":"downloads"}]}'
Invoke-RestMethod -Uri http://127.0.0.1:6800/jsonrpc -Method Post -ContentType 'application/json' -Body $body
```

`aria2.addUri` 返回 16 位十六进制 GID。后续用 `aria2.tellStatus` 查询：

```json
{"jsonrpc":"2.0","id":2,"method":"aria2.tellStatus","params":["0123456789abcdef",["gid","status","totalLength","completedLength","downloadSpeed","dir"]]}
```

### 批量请求

HTTP JSON-RPC 接受请求数组。多个只读状态请求也可以使用 aria2 的 `system.multicall`：

```json
{"jsonrpc":"2.0","id":3,"method":"system.multicall","params":[[
  {"methodName":"aria2.getVersion","params":[]},
  {"methodName":"aria2.getGlobalStat","params":[]}
]]}
```

`system.multicall` 不允许递归调用自身。通知请求可以省略 `id`，此时不返回 JSON-RPC 结果。

### GET 查询

`GET /jsonrpc` 支持 JSON 参数查询，也支持 JSONP 形式的 `jsoncallback`；WebSocket 升级同样必须使用 `/jsonrpc`。生产环境建议使用 POST 或 WebSocket。

## 4. XML-RPC

XML-RPC 使用 `POST /rpc`，方法名和 aria2 参数顺序与 JSON-RPC 相同：

```xml
<?xml version="1.0"?>
<methodCall>
  <methodName>aria2.getVersion</methodName>
  <params></params>
</methodCall>
```

XML-RPC 返回标准 `methodResponse`。请求体同样受 `rpc-max-request-size` 限制。

## 5. WebSocket 与事件

连接 `ws://host:port/jsonrpc` 后发送普通 JSON-RPC 请求；服务端会在下载生命周期中推送 JSON-RPC 通知。事件参数是包含 `gid` 的对象：

```json
{"jsonrpc":"2.0","method":"aria2.onDownloadComplete","params":[{"gid":"0123456789abcdef"}]}
```

基础事件：`aria2.onDownloadStart`、`aria2.onDownloadPause`、`aria2.onDownloadStop`、`aria2.onDownloadComplete`、`aria2.onDownloadError`。启用 BitTorrent 时还会有 `aria2.onBtDownloadComplete`。默认 ping 间隔为 30 秒，pong 超时为 60 秒；客户端应响应 ping 并在断线后重新连接。

## 6. 方法参考

所有参数均为 JSON 数组中的位置参数；方法名大小写敏感。可选的 `options` 是字符串键值对象，值可以是字符串、数字、布尔值；数组型累积选项也支持数组表示。

### 任务创建与队列

| 方法 | 参数 | 返回 |
| --- | --- | --- |
| `aria2.addUri` | `uris`, `options?`, `position?` | GID |
| `aria2.addTorrent` | base64 torrent, `uris?`, `options?`, `position?` | GID |
| `aria2.addMetalink` | base64 metalink, `options?` | GID 数组；需 Metalink feature |
| `aria2.remove` / `forceRemove` | `gid` | GID |
| `aria2.pause` / `forcePause` / `unpause` | `gid` | GID |
| `aria2.pauseAll` / `forcePauseAll` / `unpauseAll` | 无 | `OK` |
| `aria2.changePosition` | `gid`, `pos`, `how` (`POS_SET`/`POS_CUR`/`POS_END`) | 新位置 |
| `aria2.changeUri` | `gid`, `fileIndex`, `delUris`, `addUris` | `OK` |

### 状态与文件

| 方法 | 参数 | 返回 |
| --- | --- | --- |
| `aria2.tellStatus` | `gid`, `keys?` | 状态对象 |
| `aria2.tellActive` | `keys?`, `token?` | 状态对象数组 |
| `aria2.tellWaiting` / `tellStopped` | `offset`, `num`, `keys?` | 状态对象数组 |
| `aria2.getUris` | `gid` | URI 对象数组 |
| `aria2.getFiles` | `gid` | 文件对象数组 |
| `aria2.getServers` | `gid` | 服务器对象数组；通常仅 active 任务可用 |
| `aria2.getPeers` | `gid` | peer 对象数组；需 BitTorrent |
| `aria2.getGlobalStat` | 无 | 全局速度和任务计数 |

常用 `tellStatus` key：`gid`、`status`、`totalLength`、`completedLength`、`uploadLength`、`downloadSpeed`、`uploadSpeed`、`pieceLength`、`numPieces`、`connections`、`errorCode`、`errorMessage`、`followedBy`、`following`、`belongsTo`、`dir`、`files`、`bittorrent`、`infoHash`。

### 选项、会话与进程

| 方法 | 参数 | 返回 |
| --- | --- | --- |
| `aria2.getOption` | `gid` | 该任务的选项对象 |
| `aria2.changeOption` | `gid`, `options` | `OK` |
| `aria2.getGlobalOption` | 无 | 全局选项对象 |
| `aria2.changeGlobalOption` | `options` | `OK` |
| `aria2.getVersion` | 无 | `version`、`enabledFeatures` |
| `aria2.getSessionInfo` | 无 | `sessionId` |
| `aria2.saveSession` | 无 | `OK` |
| `aria2.removeDownloadResult` | `gid` | `OK` |
| `aria2.purgeDownloadResult` | 无 | `OK` |
| `aria2.shutdown` / `forceShutdown` | 无 | `OK` |

### 系统方法

`system.listMethods` 返回当前构建实际支持的方法；`system.listNotifications` 返回事件名；`system.multicall` 接收 `[{"methodName":"...","params":[...]}]` 数组。

当前基础方法完整名称为：`aria2.addUri`、`aria2.remove`、`aria2.pause`、`aria2.forcePause`、`aria2.pauseAll`、`aria2.forcePauseAll`、`aria2.unpause`、`aria2.unpauseAll`、`aria2.forceRemove`、`aria2.changePosition`、`aria2.tellStatus`、`aria2.getUris`、`aria2.getFiles`、`aria2.getServers`、`aria2.tellActive`、`aria2.tellWaiting`、`aria2.tellStopped`、`aria2.getOption`、`aria2.changeUri`、`aria2.changeOption`、`aria2.getGlobalOption`、`aria2.changeGlobalOption`、`aria2.purgeDownloadResult`、`aria2.removeDownloadResult`、`aria2.getVersion`、`aria2.getSessionInfo`、`aria2.shutdown`、`aria2.forceShutdown`、`aria2.getGlobalStat`、`aria2.saveSession`、`system.multicall`、`system.listMethods`、`system.listNotifications`。按 feature 增加 `aria2.addTorrent`、`aria2.getPeers`、`aria2.addMetalink`。

## 7. 错误与限制

JSON-RPC 解析错误为 `-32700`，无效请求为 `-32600`，方法不存在为 `-32601`，参数错误为 `-32602`，内部错误为 `-32603`。认证失败使用 aria2 兼容错误码 `1`。HTTP 错误通常对应 `400`；认证失败对应 `401`。默认单个请求体上限为 2 MiB，可用 `rpc-max-request-size` 调整。

## 8. HTTPS、CORS 与上传

```ini
rpc-secure=true
rpc-certificate=server.crt
rpc-private-key=server.key
rpc-cors-domain=https://panel.example.com
rpc-save-upload-metadata=true
```

证书和私钥必须是 PEM。CORS 应设置为明确域名，多个域名以逗号分隔；`rpc-allow-origin-all=true` 会允许所有来源，仅适合受控环境。上传的 torrent/metadata 受请求体限制，并由 `rpc-save-upload-metadata` 控制是否保存。

## 9. 客户端排查顺序

1. 用 `aria2.getVersion` 确认 URL、认证和服务是否可达。
2. 用 `system.listMethods`/`system.listNotifications` 检查 feature 能力。
3. 用 `aria2.addUri` 创建任务并保存 GID。
4. 用 `tellStatus` 轮询，或监听 WebSocket 事件。
5. 先检查 JSON-RPC `error.code`，再读取 `result`；不要只依据 HTTP 200 判断业务成功。
