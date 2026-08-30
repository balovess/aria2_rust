# aria2-rust RPC Guide

中文版本：[`rpc-guide-cn.md`](rpc-guide-cn.md)

This is the standalone RPC reference. The RPC service is started by `aria2c` and is disabled by default. It provides JSON-RPC 2.0, XML-RPC, and WebSocket transports. The exact method set depends on build features; use `system.listMethods` to inspect the running build.

## 1. Starting the server

Minimal local configuration:

```ini
enable-rpc=true
rpc-listen-address=127.0.0.1
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
```

```text
aria2c --conf-path=aria2.conf
```

The default address is `127.0.0.1:6800`. HTTP JSON-RPC is available at `http://127.0.0.1:6800/jsonrpc`, XML-RPC at `http://127.0.0.1:6800/rpc`, and WebSocket at `ws://127.0.0.1:6800/jsonrpc`.

For remote access, set `rpc-listen-all=true` or use an explicit `rpc-listen-address`, and also configure `rpc-secret` and a firewall. Do not expose unauthenticated RPC to the public internet.

## 2. Authentication

`rpc-secret` is the recommended authentication method. The token is the first item in the `params` array and is removed before method-specific parsing:

```json
{"jsonrpc":"2.0","id":1,"method":"aria2.getVersion","params":["token:replace-with-a-long-random-token"]}
```

The token may be omitted when no secret is configured. `rpc-user` and `rpc-passwd` provide deprecated Basic Auth compatibility. Clients may also send an `Authorization: Basic ...` header. The secret is never returned by `getGlobalOption`.

## 3. JSON-RPC

### Single request

```powershell
$body = '{"jsonrpc":"2.0","id":1,"method":"aria2.addUri","params":[["https://example.com/file.zip"],{"dir":"downloads"}]}'
Invoke-RestMethod -Uri http://127.0.0.1:6800/jsonrpc -Method Post -ContentType 'application/json' -Body $body
```

`aria2.addUri` returns a 16-character hexadecimal GID. Query it with `aria2.tellStatus`:

```json
{"jsonrpc":"2.0","id":2,"method":"aria2.tellStatus","params":["0123456789abcdef",["gid","status","totalLength","completedLength","downloadSpeed","dir"]]}
```

### Batch requests

HTTP JSON-RPC accepts an array of requests. Read-only status calls can also be grouped with `system.multicall`:

```json
{"jsonrpc":"2.0","id":3,"method":"system.multicall","params":[[{"methodName":"aria2.getVersion","params":[]},{"methodName":"aria2.getGlobalStat","params":[]}]]}
```

`system.multicall` cannot call itself recursively. A notification may omit `id`; it does not produce a JSON-RPC result.

### GET requests

`GET /jsonrpc` accepts query-form JSON parameters and JSONP through `jsoncallback`. WebSocket upgrades also use `/jsonrpc`. POST or WebSocket is recommended for production clients.

## 4. XML-RPC

XML-RPC uses `POST /rpc`; method names and parameter order are the same as JSON-RPC:

```xml
<?xml version="1.0"?>
<methodCall>
  <methodName>aria2.getVersion</methodName>
  <params></params>
</methodCall>
```

Responses use the standard `methodResponse` format. XML request bodies are subject to `rpc-max-request-size`.

## 5. WebSocket and events

Connect to `ws://host:port/jsonrpc` and send normal JSON-RPC requests. The server pushes JSON-RPC notifications for download lifecycle events:

```json
{"jsonrpc":"2.0","method":"aria2.onDownloadComplete","params":[{"gid":"0123456789abcdef"}]}
```

Base events are `aria2.onDownloadStart`, `aria2.onDownloadPause`, `aria2.onDownloadStop`, `aria2.onDownloadComplete`, and `aria2.onDownloadError`. BitTorrent builds also provide `aria2.onBtDownloadComplete`. The default ping interval is 30 seconds and the pong timeout is 60 seconds; clients should answer pings and reconnect after a disconnect.

## 6. Method reference

All parameters are positional items in the JSON-RPC `params` array. Optional `options` values are string-keyed objects. Values may be strings, numbers, or booleans; cumulative options also accept arrays.

### Task creation and queue

| Method | Parameters | Result |
| --- | --- | --- |
| `aria2.addUri` | `uris`, `options?`, `position?` | GID |
| `aria2.addTorrent` | base64 torrent, `uris?`, `options?`, `position?` | GID |
| `aria2.addMetalink` | base64 metalink, `options?` | GID array; requires Metalink |
| `aria2.remove` / `forceRemove` | `gid` | GID |
| `aria2.pause` / `forcePause` / `unpause` | `gid` | GID |
| `aria2.pauseAll` / `forcePauseAll` / `unpauseAll` | none | `OK` |
| `aria2.changePosition` | `gid`, `pos`, `how` (`POS_SET`/`POS_CUR`/`POS_END`) | New position |
| `aria2.changeUri` | `gid`, `fileIndex`, `delUris`, `addUris` | `OK` |

### Status and files

| Method | Parameters | Result |
| --- | --- | --- |
| `aria2.tellStatus` | `gid`, `keys?` | Status object |
| `aria2.tellActive` | `keys?`, `token?` | Status object array |
| `aria2.tellWaiting` / `tellStopped` | `offset`, `num`, `keys?` | Status object array |
| `aria2.getUris` | `gid` | URI object array |
| `aria2.getFiles` | `gid` | File object array |
| `aria2.getServers` | `gid` | Server object array; normally active tasks only |
| `aria2.getPeers` | `gid` | Peer object array; requires BitTorrent |
| `aria2.getGlobalStat` | none | Global speeds and task counts |

Common `tellStatus` keys include `gid`, `status`, `totalLength`, `completedLength`, `uploadLength`, `downloadSpeed`, `uploadSpeed`, `pieceLength`, `numPieces`, `connections`, `errorCode`, `errorMessage`, `followedBy`, `following`, `belongsTo`, `dir`, `files`, `bittorrent`, and `infoHash`.

### Options, session, and process

| Method | Parameters | Result |
| --- | --- | --- |
| `aria2.getOption` | `gid` | Per-task options |
| `aria2.changeOption` | `gid`, `options` | `OK` |
| `aria2.getGlobalOption` | none | Global options |
| `aria2.changeGlobalOption` | `options` | `OK` |
| `aria2.getVersion` | none | `version`, `enabledFeatures` |
| `aria2.getSessionInfo` | none | `sessionId` |
| `aria2.saveSession` | none | `OK` |
| `aria2.removeDownloadResult` | `gid` | `OK` |
| `aria2.purgeDownloadResult` | none | `OK` |
| `aria2.shutdown` / `forceShutdown` | none | `OK` |

### System methods

`system.listMethods` returns methods supported by the current build. `system.listNotifications` returns event names. `system.multicall` accepts an array of `{"methodName":"...","params":[...]}` objects.

The complete base method catalog is: `aria2.addUri`, `aria2.remove`, `aria2.pause`, `aria2.forcePause`, `aria2.pauseAll`, `aria2.forcePauseAll`, `aria2.unpause`, `aria2.unpauseAll`, `aria2.forceRemove`, `aria2.changePosition`, `aria2.tellStatus`, `aria2.getUris`, `aria2.getFiles`, `aria2.getServers`, `aria2.tellActive`, `aria2.tellWaiting`, `aria2.tellStopped`, `aria2.getOption`, `aria2.changeUri`, `aria2.changeOption`, `aria2.getGlobalOption`, `aria2.changeGlobalOption`, `aria2.purgeDownloadResult`, `aria2.removeDownloadResult`, `aria2.getVersion`, `aria2.getSessionInfo`, `aria2.shutdown`, `aria2.forceShutdown`, `aria2.getGlobalStat`, `aria2.saveSession`, `system.multicall`, `system.listMethods`, and `system.listNotifications`. Features add `aria2.addTorrent`, `aria2.getPeers`, and `aria2.addMetalink` as applicable.

## 7. Errors and limits

JSON-RPC parse error is `-32700`, invalid request is `-32600`, method not found is `-32601`, invalid parameters are `-32602`, and internal error is `-32603`. Authentication failure uses aria2-compatible code `1`. HTTP errors generally use `400`; authentication failure uses `401`. The default request body limit is 2 MiB and can be changed with `rpc-max-request-size`.

## 8. HTTPS, CORS, and uploads

```ini
rpc-secure=true
rpc-certificate=server.crt
rpc-private-key=server.key
rpc-cors-domain=https://panel.example.com
rpc-save-upload-metadata=true
```

The certificate and private key must be PEM files. Set CORS to explicit origins, separated by commas. `rpc-allow-origin-all=true` allows every origin and is intended only for controlled environments. Uploaded torrent and metadata bodies are limited by `rpc-max-request-size`; `rpc-save-upload-metadata` controls whether they are saved.

## 9. Troubleshooting order

1. Call `aria2.getVersion` to verify the URL, authentication, and server reachability.
2. Call `system.listMethods` and `system.listNotifications` to check feature support.
3. Create a task with `aria2.addUri` and retain its GID.
4. Poll with `tellStatus`, or subscribe to WebSocket events.
5. Check JSON-RPC `error.code` before reading `result`; HTTP 200 alone does not prove business success.
