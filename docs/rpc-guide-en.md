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

### Startup mode difference

aria2-rust makes RPC binding part of a startup plan instead of allowing `enable-rpc=true` in a shared configuration to affect every command-line invocation unconditionally:

- With no download input, `enable-rpc=true` starts an RPC-only service and keeps the process alive.
- With a URI, URI list, torrent, Metalink, or session work, `enable-rpc=true` from the configuration file or environment does not start an RPC listener for that invocation; the process exits after the downloads finish.
- To download and accept remote tasks at the same time, explicitly pass `--enable-rpc=true` on the command line.
- `daemon=true` only changes whether the process is detached; it does not change the mode selection above.

This intentionally differs from the C++ original aria2, which creates its listener from the final `enable-rpc` value. The aria2-rust policy is designed for reusing one configuration between a background service and occasional one-shot commands. It prevents the shared configuration from causing a port conflict, but it also means a download command with initial input will not enter Download + RPC mode from configuration alone.

The original C++ aria2 sets `SO_REUSEADDR` on its listener. On Windows and some other systems, this can allow multiple processes to listen on the same address and port; its IPv4/IPv6 fallback can also hide occupancy in one address family. This does not mean that processes share task or RPC state, and it does not guarantee that a request reaches the intended process. aria2-rust's explicit RPC mode does not use this cross-process reuse: it tries the address families allowed by `disable-ipv6` and reports a startup error when all binds fail. For a stable single RPC service, run one RPC-only background process with no initial download input.

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

GID parameters also accept a unique high-order hexadecimal prefix, matching
aria2's `GroupId::expandUnique` behavior. Ambiguous prefixes are rejected.
GIDs use hexadecimal digits only (no `0x` prefix). A malformed, missing, or
ambiguous GID is reported as aria2's execution error (code `1`), with the
diagnostic distinguishing `Invalid GID`, `is not found`, and `is not unique`.

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

### 6.1 Upstream compatibility matrix

The compatibility baseline is the official C++ aria2 1.37.0 JSON-RPC/XML-RPC contract. “Upstream” means that method names, parameter order, result shape, wire types, and error semantics remain compatible. Extension methods may be added, but they must not change responses from upstream methods.

| Scope | Upstream baseline | aria2-rust | Conclusion |
| --- | --- | --- | --- |
| Core tasks/queue | `addUri`, `remove`, `pause`, `unpause`, `changePosition`, `changeUri`, etc. | Provided | Compatibility baseline |
| Status queries | `tellStatus`, `tellActive`, `tellWaiting`, `tellStopped` | Provided, including `keys` projection | Compatibility baseline |
| Files/URIs/servers | `getFiles`, `getUris`, `getServers` | Provided | Compatibility baseline |
| BT task status | `infoHash`, `numSeeders`, `seeder`, `bitfield`, `pieceLength`, `numPieces`, `connections`, etc. | Provided | Compatibility baseline |
| BT peers | Standard `getPeers` fields and string wire types | Provided; discovery source is internal only | Compatibility baseline |
| BT peer connection counts | No dedicated upstream RPC | `getPeerStats` | Extension |
| BT peer details | Keep upstream `getPeers` unchanged | `getPeerDetails` | Extension |
| Global statistics | `getGlobalStat` speeds and task counts | Provided | Compatibility baseline |
| Version/options/session/system | Corresponding upstream methods | Provided | Compatibility baseline |
| DHT internals | No dedicated upstream RPC | `getDhtStatus` | Extension |
| Manual DHT maintenance | No dedicated upstream RPC | `saveDhtState`, `evictDhtNodes` | Extension |
| Tracker runtime snapshot | Upstream has no `getTrackers` | `getTrackers` | Extension |
| Browser session context | Not present upstream | `updateBrowserContext`, `clearBrowserContext` | Extension |

Upstream method responses must not contain extension fields. Runtime data such as `source`, `completedPieces`, and `missingPieces` may still be maintained internally, but is not emitted by upstream `getPeers`/`tellStatus`; extension data belongs in separate extension methods. Numbers and booleans continue to use aria2's string wire format.

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

`changeUri` uses a one-based `fileIndex` and applies to the addressed file
entry even when that file is currently unselected. It returns the number of
deleted and added URIs as string-valued numbers.

The `uris` parameter of `addTorrent` supplies additional WebSeed endpoints.
They are merged, sorted, and deduplicated with the torrent's `url-list`; they
are not Tracker URLs. Single- and multi-file torrents expand endpoints against
their file layout. Each piece request reads only the overlapping file ranges,
and HTTP clients are reused by origin. The BT session schedules up to four
WebSeed piece requests concurrently with peer downloads, using exclusive piece
reservations. Network workers only fetch data; the BT session remains the sole
owner of hash verification, writing, and completion accounting. A piece is
scheduled this way only when WebSeeds cover all its file-backed ranges;
partially covered pieces still use peers and the failure fallback. This is a
coarser scheduling unit than upstream's per-file segments sharing PieceStorage.
For an active task, `changeUri` updates the affected WebSeed queue, resets the
scheduler scan, and wakes an idle scheduler; requests already in flight are
not forcibly canceled. Failed pieces are retried up to the task's `max-tries`
limit, observing `retry-wait`; peers can still fetch those pieces concurrently.
A URI change triggers a new scan and resets WebSeed retry counts.

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
| `aria2.getPeerStats` | `gid` | Current connected peer/seeder/leecher counts; requires BitTorrent |
| `aria2.getPeerDetails` | `gid` | Detailed current peer snapshot; requires BitTorrent |
| `aria2.getTrackers` | `gid` | Per-URL tracker runtime snapshot; requires BitTorrent |
| `aria2.getDhtStatus` | none | Aggregate DHT status for active BT/magnet tasks; requires BitTorrent |
| `aria2.saveDhtState` | none | Immediately save routing tables and BEP 44 items for active DHT engines; requires BitTorrent |
| `aria2.evictDhtNodes` | none | Immediately evict bad nodes and try cached replacements; returns `[evicted, replacement_attempts]`; requires BitTorrent |
| `aria2.getGlobalStat` | none | Global speeds and task counts |

`aria2.getFiles(gid)` is the standard query for file paths, total lengths,
completed lengths, and URI metadata. Numeric fields are serialized as strings
to match the aria2 RPC contract. It queries an existing task; it is not a
standalone URL-inspection method:

- HTTP/FTP tasks need to complete their metadata probe before the length is known.
- A local torrent is parsed by `addTorrent`, so its file list is available even with `pause=true`.
- A magnet link needs metadata exchange before its file names and lengths are available.
- `dry-run` only performs HTTP/FTP availability and length probing; it is not a general file-list API.

`aria2.getUris(gid)` follows the original RPC implementation and returns the
URI entries associated with the first `FileEntry` only. For a multi-file task,
use `getFiles` to inspect each file's URI list; `getUris` does not flatten URI
entries from all files.

The Python and Node.js bindings expose this as `client.get_files(gid)` and
`client.getFiles(gid)` respectively.

Common `tellStatus` keys include `gid`, `status`, `totalLength`, `completedLength`, `uploadLength`, `downloadSpeed`, `uploadSpeed`, `pieceLength`, `numPieces`, `connections`, `errorCode`, `errorMessage`, `followedBy`, `following`, `belongsTo`, `dir`, `files`, `bittorrent`, and `infoHash`.

BitTorrent status details: `bittorrent` is a nested torrent metadata object. It
contains tiered `announceList` and, when present, `comment`, `creationDate`,
`mode`, and `info.name`. Piece progress is exposed through `bitfield`,
`pieceLength`, and `numPieces`; BT runtime statistics also include `seeder`,
`numSeeders`, `verifiedLength`, and `verifyIntegrityPending`. `numSeeders` is
the number of currently connected seeder peers; it is not the Tracker swarm's
`complete` count. `verifiedLength` and `verifyIntegrityPending` describe
integrity-check progress and queued verification state, respectively.
`completedPieces` and `missingPieces` are internal runtime statistics and are
not part of the upstream `tellStatus` wire response. Lengths, speeds, and
counters that belong to the aria2-compatible status contract are serialized as
strings on the wire.

`aria2.getPeers` reports currently active connections, not historical peers. In
addition to the standard `peerId`, `ip`, `port`, `amChoking`, `peerChoking`,
`downloadSpeed`, and `seeder` fields, `bitfield` is the peer's raw piece
bitfield encoded as lowercase hexadecimal. The field is always present; an
empty string means the peer bitfield is not known. The first discovery source
(`tracker`, `dht`, `pex`, `lpd`, `incoming`, or
`unknown`) is retained internally and is not emitted in the upstream response.
Port, speed, boolean, and seeder values follow aria2's string wire format.

`aria2.getPeerStats(gid)` returns `{ "peerCount": "...", "seeders": "...", "leechers": "...", "unknown": "..." }`.
Counts cover currently active connections; unknown seeder state is counted separately
as `unknown`, not as a leecher. Counts use aria2's string wire format. This is a Rust extension, not an
upstream aria2 RPC. It differs from tracker-reported whole-swarm counts in `getTrackers`
and does not report DHT nodes.

`aria2.getPeerDetails(gid)` is a separate extension and does not change upstream
`getPeers` wire data. It returns the same active-peer snapshot with `source`,
`progressPercent`, cumulative `uploadedBytes`/`downloadedBytes`, current and lifetime
average speeds, local-perspective structured `flags`, and directional request counts.
Byte counters are decimal strings; average speeds are lifetime bytes divided by connection
age. Values without an authoritative source are omitted: `client` is absent until a BEP 10
client name is received. `outstandingRequestsToPeer` counts requests sent to a peer whose
responses are still pending; `outstandingRequestsFromPeer` counts peer upload blocks still
queued locally. Neither is cumulative. `progressPercent` is
calculated from the peer's advertised bitfield and torrent piece count, omitted if the
bitfield is unknown; seeders report 100%.

`aria2.getTrackers` returns a point-in-time runtime snapshot for the specified
GID. It is not a claim that every tracker is currently live or online. Each
entry contains `uri`, 1-based `tier`, `current`, `lastAttempt`, `announceReady`,
`allFailed`, `inFlight`, `interval`, `minInterval`, `seeders`, `leechers`,
`trackerId`, and optional `secondsSinceLastSuccess`. The snapshot is published
by the executing BitTorrent command and is removed when that command exits.
`current` identifies the next tracker selected by the announce state machine;
`lastAttempt` identifies the most recently attempted tracker. In this extension
interface `interval` is serialized as a string; other tracker numbers and
boolean state use native JSON types. Each URL also includes `status`,
`snapshotAtUnixMillis`, optional `lastSuccessAtUnixMillis`, and optional
`downloaded`. Timestamps are decimal Unix milliseconds; `downloaded` is the
Tracker-reported completed torrent count, not bytes, and is present only when supplied
by an HTTP bencoded response. `seeders` and `leechers` are omitted when the tracker did
not provide the corresponding valid `complete` or `incomplete` value; an explicit zero is
preserved as zero. The scheduling flags describe
announce state, not a universal realtime/online status for all URLs.

`aria2.getDhtStatus` is process-wide. It aggregates the DHT engines registered
by active BT/magnet commands and returns `state` (`stopped`, `bootstrapping`,
`running`, or `shuttingDown`) plus `totalNodes`, `goodNodes`,
`pendingTransactions`, `questionableNodes`, `badNodes`, `cachedNodes`, and
`bucketCount`. It also reports `persistenceEnabled`,
`persistenceMaxAgeSecs`, `cleanupIntervalSecs`, and `saveIntervalSecs` for
the active DHT configuration. Numeric fields use aria2's string wire format;
`persistenceEnabled` is a native JSON boolean. With no active DHT engine, the
result is `stopped` with zero counters and persistence disabled. `totalNodes`/
`goodNodes` are DHT routing-table node counts; they are not BitTorrent seeder
counts.

`aria2.saveDhtState` and `aria2.evictDhtNodes` take no parameters. The save
operation reuses the automatic save chain's serialization lock, routing-table
merge, and BEP 44 persistence logic. The eviction operation reuses the
periodic cleanup path's bad-node eviction and cached replacement logic. With
no active DHT engine they return an execution error. They affect only DHT
engines registered in the current process and do not change `dht-*` options;
automatic maintenance continues on its configured schedule.

Public tracker catalog settings are global options and can be inspected or
changed through the normal option methods. The standard defaults are:

```json
{
  "enable-public-trackers": "true",
  "bt-tracker-source": "https://cf.trackerslist.com/best.txt",
  "bt-tracker-update-interval": "86400"
}
```

The catalog is refreshed periodically and newly available URLs are appended
after torrent/user tracker tiers with duplicates removed. See the
[configuration guide](configuration-guide-en.md#public-tracker-catalog) for
the exclusion and multi-source rules.

The response extension `announce-list` is also parsed and appended as new tiers.
Duplicate URLs are discarded, and the torrent, user, and public-catalog tiers are
never replaced. This makes torrent metadata, user configuration, the public
catalog, and tracker-discovered URLs one observable discovery chain.

`getTrackers` is the tracker health surface: `allFailed`, `announceReady`,
`lastAttempt`, `secondsSinceLastSuccess`, and `inFlight` expose per-task health
and current request concurrency. Public-catalog source fetching is internally
bounded to four concurrent sources, preventing an unbounded refresh fan-out;
this guard does not change the original tracker RPC response.

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
| `aria2.removeDownloadFiles` | `gid` | `OK` |
| `aria2.purgeDownloadResult` | none | `OK` |
| `aria2.shutdown` / `forceShutdown` | none | `OK` |

`aria2.removeDownloadFiles(gid)` removes the selected output files for that stopped task and recursively for its `followedBy` children. Any task that has not reached the stopped-results list is rejected, including paused tasks. The stopped result is retained; missing output files are treated as already removed. Directories and `.aria2` control files are left in place. If a descendant's stopped result has already been purged, cleanup returns an error because its output paths are no longer known.

### Browser session context

| Method | Parameters | Result |
| --- | --- | --- |
| `aria2.updateBrowserContext` | `context`: `cookie?`, `user_agent?`, `headers?` | `OK` |
| `aria2.clearBrowserContext` | none | `OK` |

These methods let external developers update browser session data while downloads are running. `context` is a complete snapshot: `cookie` and `user_agent` are strings, and `headers` is an array of `[name, value]` pairs. Each update replaces the complete snapshot, so a bridge should merge changes and publish all credentials that are still valid. Both methods require RPC authentication and affect HTTP downloads using the process-wide browser context.

```json
{"jsonrpc":"2.0","id":4,"method":"aria2.updateBrowserContext","params":["token:replace-with-a-long-random-token",{"cookie":"sid=abc","user_agent":"Mozilla/5.0","headers":[["X-Signature","signed-value"]]}]}
```

The same request can be sent through `POST /jsonrpc` or the existing `ws://host:port/jsonrpc` WebSocket transport; no second bridge protocol is required. See the [browser session bridge developer guide](browser-bridge-guide-en.md) for standalone-process, browser-extension, and CDP client examples.

### System methods

`system.listMethods` returns methods supported by the current build. `system.listNotifications` returns event names. `system.multicall` accepts an array of `{"methodName":"...","params":[...]}` objects.

The complete base method catalog is: `aria2.addUri`, `aria2.remove`, `aria2.pause`, `aria2.forcePause`, `aria2.pauseAll`, `aria2.forcePauseAll`, `aria2.unpause`, `aria2.unpauseAll`, `aria2.forceRemove`, `aria2.changePosition`, `aria2.tellStatus`, `aria2.getUris`, `aria2.getFiles`, `aria2.getServers`, `aria2.tellActive`, `aria2.tellWaiting`, `aria2.tellStopped`, `aria2.getOption`, `aria2.changeUri`, `aria2.changeOption`, `aria2.getGlobalOption`, `aria2.changeGlobalOption`, `aria2.purgeDownloadResult`, `aria2.removeDownloadResult`, `aria2.removeDownloadFiles`, `aria2.getVersion`, `aria2.getSessionInfo`, `aria2.shutdown`, `aria2.forceShutdown`, `aria2.getGlobalStat`, `aria2.saveSession`, `aria2.updateBrowserContext`, `aria2.clearBrowserContext`, `system.multicall`, `system.listMethods`, and `system.listNotifications`. Features add `aria2.addTorrent`, `aria2.getPeers`, `aria2.getPeerStats`, `aria2.getPeerDetails`, `aria2.getTrackers`, `aria2.getDhtStatus`, `aria2.saveDhtState`, `aria2.evictDhtNodes`, and `aria2.addMetalink` as applicable; an all-features build exposes 45 methods.

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

## 10. Official differential testing

The repository provides an official-aria2c differential harness at `scripts/rpc-differential.ps1`. It starts the official aria2c and the current build separately, then compares JSON-RPC, XML-RPC, WebSocket, task status, field omission, error codes, and a paused minimal BitTorrent torrent for result shape and wire types.

Provide an official C++ aria2c binary and run:

```powershell
$env:ARIA2_ORIGINAL_BIN = 'C:\tools\aria2c-original.exe'
cargo build -p aria2 --features standard --bin aria2c
pwsh -File .\scripts\rpc-differential.ps1 -BuildCurrent
```

Paths may also be passed explicitly:

```powershell
pwsh -File .\scripts\rpc-differential.ps1 `
  -Original C:\tools\aria2c-original.exe `
  -Current .\target\debug\aria2c.exe
```
