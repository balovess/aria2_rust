# aria2-rust 参数配置使用说明

English version: [`configuration-guide-en.md`](configuration-guide-en.md)

本文独立说明命令行、配置文件和 RPC 选项。选项注册表是唯一的类型和校验来源；`aria2c --help`、配置文件和 RPC 的选项校验共享这套定义。

## 1. 配置来源与优先级

生效顺序从低到高为：内置默认值 < 环境变量 < 配置文件 < 命令行。命令行显式指定的值覆盖配置文件；布尔值必须显式写成 `--option`、`--option=true` 或 `--option=false`，也支持 `--no-option` 关闭普通布尔选项。

```text
aria2c --conf-path=aria2.conf https://example.com/file.iso
aria2c --no-conf https://example.com/file.iso
aria2c --check-config --conf-path=aria2.conf
```

相对路径相对于进程工作目录。配置文件中的空行、`#` 和 `;` 开头行会被忽略；格式是每行一个 `name=value`。已知但当前运行时未实现的兼容项会保留并发出警告，不会改变下载行为；未知项也会警告。重复出现的累积选项按定义追加，其余选项以后出现的值覆盖以前的值。

## 2. 启动模式：与原版 aria2 的明确差异

aria2-rust 不直接把合并后的 `enable-rpc` 值当作“本次进程一定要监听 RPC”。启动阶段会先生成一个启动计划，将 daemon（进程是否脱离终端）与下载/RPC 业务模式分开：

| 启动条件 | aria2-rust 模式 | RPC 行为 |
| --- | --- | --- |
| 没有下载输入，`enable-rpc=true` | RPC-only | 启动 RPC 并等待远程任务 |
| 有 URI、`@uri-list`、URI 列表、torrent、Metalink 或 session 恢复任务 | 一次性下载 | 忽略仅来自配置文件/环境的 `enable-rpc` |
| 有下载输入，且命令行显式 `--enable-rpc=true` | 下载 + RPC | 下载期间接受 RPC；初始任务完成后仍保持 RPC 服务，直到 RPC shutdown 或进程停止 |
| 命令行显式 `--enable-rpc=false` | 禁止 RPC | 覆盖其他来源的 RPC 设置 |

这是产品设计上的行为差异，不是对 C++ 原版行为的声称。原版 aria2 通常依据最终的 `enable-rpc` 值创建 RPC listener，因此同一份含 `enable-rpc=true` 的配置在原版中可让一次性命令行下载同时监听 RPC。aria2-rust 选择按用户是否明确启动下载来决定进程生命周期，避免后台 RPC 服务与临时命令行共用配置时发生端口冲突。

在原版 C++ aria2 中，监听 socket 会设置 `SO_REUSEADDR`。在 Windows 等系统上，这可能允许多个进程以相同地址和端口进入监听状态；IPv4/IPv6 的地址族回退也可能让端口冲突暂时不明显。这不是多个 aria2 进程共享同一个 RPC 服务：请求最终由哪个进程接收并不适合作为应用层契约。aria2-rust 的显式 RPC 模式不依赖这种跨进程端口复用；它会按 `disable-ipv6` 尝试可用的地址族，若所有尝试都失败则启动失败。需要保证 RPC 服务唯一时，应使用 RPC-only 后台进程，并让命令行下载走共享配置的默认一次性模式。

`daemon=true` 只负责将进程放到后台，不会自动打开 RPC，也不会把一次性下载变成长驻服务。要运行后台 RPC 服务，应使用没有初始下载输入的配置并设置 `enable-rpc=true`。

### `daemon` 与 `enable-rpc` 的关系

这两个选项不冲突，但控制的是不同的事情：

| 配置 | 作用 | 不会做什么 |
| --- | --- | --- |
| `daemon=true` | 让当前进程脱离终端并在后台运行 | 不会自动启动 RPC |
| `enable-rpc=true` | 在适用的启动模式中开启 RPC listener | 不会自动让进程后台运行 |
| 两者同时为 `true` | 后台运行 RPC-only 服务，或后台运行显式请求的“下载 + RPC”进程 | 不会让普通下载自动继承 RPC |

因此，共享配置中建议保留 `enable-rpc=true`，省略 `daemon=true`。启动后台服务时使用 `--daemon=true`；启动一次性下载时不要加它。否则普通下载虽然不会监听 RPC，但仍会被后台化，终端不会直接显示完整的下载输出。

## 3. 类型、单位与示例

| 类型 | 写法 |
| --- | --- |
| Boolean | `true`/`false`；CLI 可省略值表示 true |
| Integer | 十进制整数，按选项的最小/最大值校验 |
| Size | 字节数，支持 `K`、`M`、`G`、`T` 后缀 |
| String/Path | 原样字符串或路径；含空格时使用 CLI 的引号 |
| Enum | 只能使用该选项列出的值 |

时间选项统一使用秒；速度和大小选项使用字节，可写 `2M`、`512K`。配置文件不使用 `--` 前缀。

## 4. 推荐模板

### 共享配置（推荐）

这是 aria2-rust 推荐的默认使用方式：同一份配置供后台 RPC 服务和偶尔执行的一次性命令复用。不要把 `daemon=true` 写入共享配置；只在启动后台服务的命令中显式添加它。

```ini
dir=downloads
continue=true
split=4
max-connection-per-server=4
enable-rpc=true
rpc-listen-address=127.0.0.1
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
```

```text
# 后台 RPC 服务：没有初始下载输入，因此启动 RPC 并保持运行
aria2c --conf-path=aria2.conf --daemon=true

# 普通命令行下载：复用配置，但不会再次监听 RPC
aria2c --conf-path=aria2.conf https://example.com/file.iso
```

配置文件/环境变量中的 `enable-rpc=true` 只在没有下载输入时自动启动 RPC。若确实需要当前命令同时下载并提供 RPC，必须显式使用 `--enable-rpc=true`；此时至少一个允许的监听地址必须可用，不能依赖原版的 `SO_REUSEADDR` 行为。

### 普通下载

```ini
dir=downloads
continue=true
check-certificate=true
split=4
max-connection-per-server=4
max-tries=5
timeout=60
```

### 可恢复会话

```ini
input-file=aria2.session
save-session=aria2.session
save-session-interval=60
keep-unfinished-download-result=true
```

### 独立 RPC 守护进程

```ini
enable-rpc=true
rpc-listen-address=127.0.0.1
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
pid-file=aria2.pid
log=aria2.log
```

## 5. 配置维护

```text
aria2c --conf-path=aria2.conf --check-config
aria2c --conf-path=aria2.conf --repair-config
aria2c --conf-path=aria2.conf --reset-config
```

`--check-config` 只校验，不启动下载引擎。`--repair-config` 会创建不覆盖已有文件的备份，并注释掉无效行；`--reset-config` 会先备份，再写入内置默认模板。这三个操作都应显式指定 `--conf-path`。不要在 aria2c 运行期间编辑 session 文件。

## 6. 完整选项目录

以下目录覆盖当前 `OptionRegistry` 注册的 canonical 名称，但“已注册”不等于“已接线”。当前源码明确标记为 `supported: false` 的选项只用于兼容旧配置，解析时会保留并警告，不会改变运行行为。其他选项也应以当前构建的 `aria2c --help` 和实际测试为准。`max-retries`、`enable-lpd`、`dht-message-path`、`server-stat-file`、`max-downloads` 是兼容别名，分别映射到 `max-tries`、`bt-enable-lpd`、`dht-file-path`、`server-stat-of`、`max-concurrent-downloads`。

当前明确未接线的兼容选项：`interface`、`multiple-interface`、`async-dns-server`、`dns-timeout`、`enable-async-dns6`、`event-poll`、`optimize-concurrent-downloads`、`optimize-concurrent-downloads-coeffA`、`optimize-concurrent-downloads-coeffB`、`rlimit-nofile`、`select-least-used-host`、`socket-recv-buffer-size`、`dscp`、`max-http-pipelining`。这些名称可以出现在兼容配置中，但不要依赖它们提供对应功能。

### General：基础、会话、界面和日志

| 选项 | 说明 |
| --- | --- |
| `dir`, `out` | 下载目录、输出文件名 |
| `conf-path`, `no-conf` | 配置文件路径、禁用配置文件 |
| `update-check`, `update-check-interval-days` | 更新检查开关和间隔天数（1..365） |
| `input-file`, `save-session`, `save-session-interval`, `auto-save-interval` | URI/session 输入、session 保存及自动保存间隔 |
| `daemon`, `pid-file`, `gid`, `netrc-path` | 守护进程、PID、首任务 GID、netrc 路径 |
| `enable-color`, `quiet`, `dry-run`, `human-readable`, `stderr` | 输出颜色、静默、只检查、可读大小、输出到 stderr |
| `download-result`, `keep-unfinished-download-result` | 结果显示（`default`/`full`/`hide`）及保留未完成结果 |
| `truncate-console-readout`, `show-console-readout` | 控制台进度显示 |
| `log`, `log-level`, `console-log-level`, `log-backup-count`, `summary-interval` | 日志文件、日志级别、备份数量、摘要间隔 |
| `torrent-file`, `metalink-file`, `show-files` | torrent/Metalink 输入和文件列表显示 |
| `metalink-version`, `metalink-language`, `metalink-os`, `metalink-location`, `metalink-base-uri` | Metalink 选择和相对 URI 基础 |
| `follow-metalink`, `metalink-preferred-protocol`, `metalink-enable-unique-protocol` | Metalink 跟随策略（`true`/`false`/`mem`）、协议和去重 |

### General：下载行为和网络

| 选项 | 说明 |
| --- | --- |
| `allow-piece-length-change`, `always-resume`, `check-integrity`, `conditional-get` | 分片长度、断点续传、完整性和条件请求 |
| `checksum`, `enable-mmap`, `max-mmap-limit` | 校验摘要、启用 mmap、mmap 最大文件大小 |
| `deferred-input`, `disable-ipv6`, `hash-check-only`, `parameterized-uri`, `pause`, `pause-metadata` | 延迟输入、IPv6、仅校验、参数化 URI、初始暂停 |
| `remove-control-file`, `reuse-uri`, `save-not-found`, `force-sequential`, `no-netrc`, `realtime-chunk-checksum` | 控制文件、URI 重用、保存未找到结果、顺序下载、禁用 netrc、实时校验 |
| `max-download-result`, `lowest-speed-limit`, `max-file-not-found`, `no-file-allocation-limit` | 结果数、最低速度、文件未找到重试、免预分配大小阈值 |
| `uri-selector`, `stream-piece-selector`, `select-least-used-host` | URI/分片/主机选择策略 |
| `optimize-concurrent-downloads`, `optimize-concurrent-downloads-coeffA`, `optimize-concurrent-downloads-coeffB` | 动态并发及其系数 |
| `on-download-start`, `on-download-pause`, `on-download-stop`, `on-download-complete`, `on-download-error` | 生命周期 hook 命令 |
| `stop-with-process`, `startup-idle-time`, `rlimit-nofile` | 进程退出联动、启动空闲时间、文件描述符限制 |
| `async-dns`, `async-dns-server`, `dns-timeout`, `enable-async-dns6` | DNS 设置；其中部分兼容项当前未实现或已 deprecated |
| `interface`, `multiple-interface`, `event-poll` | 网卡绑定和事件轮询；当前绑定/轮询兼容项可能未实现 |
| `server-stat-timeout`, `server-stat-if`, `server-stat-of` | 服务器统计过期、输入和输出文件 |

### HTTP/FTP/SFTP

| 选项 | 说明 |
| --- | --- |
| `all-proxy`, `http-proxy`, `https-proxy`, `ftp-proxy`, `no-proxy` | 代理及不代理主机列表 |
| `all-proxy-user`, `all-proxy-passwd`, `http-proxy-user`, `http-proxy-passwd`, `https-proxy-user`, `https-proxy-passwd`, `ftp-proxy-user`, `ftp-proxy-passwd` | 代理认证 |
| `proxy-method`, `user-agent`, `referer`, `header` | 代理方法、UA、Referer、自定义请求头 |
| `load-cookies`, `save-cookies`, `http-user`, `http-passwd`, `ftp-user`, `ftp-passwd` | Cookie 和 HTTP/FTP 认证 |
| `connect-timeout`, `timeout`, `max-tries`, `retry-wait` | 连接超时、总超时、尝试次数、重试等待（秒） |
| `split`, `min-split-size`, `max-connection-per-server` | 分片、最小分片大小、每服务器连接上限 |
| `check-certificate`, `ca-certificate`, `certificate`, `private-key`, `min-tls-version` | TLS 校验、CA、客户端证书/私钥、最低 TLS 版本 |
| `allow-overwrite`, `auto-file-renaming`, `continue`, `remote-time` | 覆盖、自动改名、续传、远端时间 |
| `enable-http-keep-alive`, `enable-http-pipelining`, `max-http-pipelining`, `http-accept-gzip`, `http-auth-challenge`, `http-no-cache`, `content-disposition-default-utf8`, `use-head`, `no-want-digest-header` | HTTP 连接、流水线、压缩、认证和响应行为 |
| `ftp-pasv`, `ftp-reuse-connection`, `ftp-type`, `ssh-host-key-md` | FTP 被动模式、连接复用、传输类型、SSH 主机密钥摘要 |

### BitTorrent（启用 bittorrent feature 时）

`seed-time`, `seed-ratio`, `bt-max-peers`, `bt-request-peer-speed-limit`, `bt-max-open-files`, `bt-seed-unverified`, `bt-save-metadata`, `bt-force-encryption`, `bt-min-crypto-level`, `bt-detach-seed-only`, `bt-enable-lpd`, `lpd-listen-port`, `bt-enable-web-seed`, `enable-dht`, `dht-listen-port`, `dht-entry-point`, `dht-file-path`, `enable-peer-exchange`, `follow-torrent`, `listen-port`, `bt-prioritize-piece`, `bt-enable-hook-after-hash-check`, `bt-exclude-tracker`, `bt-external-ip`, `bt-hash-check-seed`, `bt-load-saved-metadata`, `bt-lpd-interface`, `bt-metadata-only`, `bt-remove-unselected-file`, `bt-require-crypto`, `bt-stop-timeout`, `bt-tracker`, `bt-tracker-source`, `bt-tracker-update-interval`, `enable-public-trackers`, `bt-tracker-connect-timeout`, `bt-tracker-interval`, `bt-tracker-timeout`, `bt-tracker-stopped-timeout`, `dht-message-timeout`, `enable-dht6`, `dht-listen-addr6`, `peer-id-prefix`, `peer-agent`, `select-file`, `index-out`, `bt-peer-blocklist`, `enable-utp`, `utp-listen-port`, `bt-keep-alive-interval`, `bt-timeout`, `bt-request-timeout`, `peer-connection-timeout`, `dht-entry-point-host`, `dht-entry-point-port`, `dht-entry-point6`, `dht-entry-point-host6`, `dht-entry-point-port6`, `dht-file-path6`, `dht-listen-addr`。

这些选项覆盖做种、DHT/IPv6 DHT、PEX、LPD/uTP、tracker、peer、文件选择和事件 hook。
事件 hook 选项为 `on-bt-download-complete` 和 `on-bt-download-error`。

### Advanced：磁盘、带宽和进程级限制

`file-allocation`、`secure-falloc`、`mmap-threshold`、`max-concurrent-downloads`、`max-overall-download-limit`、`max-download-limit`、`max-overall-upload-limit`、`max-upload-limit`、`piece-length`、`disk-cache`、`stop`、`force-save`、`save-server-stat-interval`、`socket-recv-buffer-size`、`dscp`、`max-resume-failure-tries`、`log-max-size`、`log-max-files`。

其中带宽/缓存/缓冲区是字节或带单位的 Size；`stop` 是停止条件；`file-allocation` 的值由当前构建帮助输出列出。不要把 `max-download-limit` 与 `max-overall-download-limit` 混用：前者针对单个任务，后者针对进程总量。

### RPC

`enable-rpc`、`rpc-listen-all`、`rpc-listen-port`（1024..65535，默认 6800）、`rpc-listen-address`（默认 `127.0.0.1`）、`rpc-secret`、`rpc-user`、`rpc-passwd`、`rpc-allow-origin`、`rpc-cors-domain`、`rpc-secure`、`rpc-certificate`、`rpc-private-key`、`rpc-allow-origin-all`、`rpc-max-request-size`（默认 2 MiB）、`rpc-save-upload-metadata`。

RPC 的完整行为和安全配置见 [`rpc-guide-cn.md`](rpc-guide-cn.md)。

## 7. 短选项

原始兼容短选项：`-a` allocation、`-c` continue、`-d` dir、`-D` daemon、`-i` input-file、`-j` max-concurrent-downloads、`-k` min-split-size、`-l` log、`-m` max-tries、`-M` metalink-file、`-n` no-netrc、`-o` out、`-O` index-out、`-p` ftp-pasv、`-P` parameterized-uri、`-q` quiet、`-R` remote-time、`-s` split、`-S` show-files、`-t` timeout、`-T` torrent-file、`-u` max-upload-limit、`-U` user-agent、`-V` check-integrity、`-x` max-connection-per-server、`-Z` force-sequential。

Rust 额外别名包括 `-B`、`-e`、`-g`、`-G`、`-I`、`-L`、`-r`、`-X`。`-h`/`--help` 和 `-v`/`--version` 是进程动作，不是配置选项。

## 8. 验证与排查

```text
aria2c --help
aria2c --help=#basic
aria2c --help=#http
aria2c --help=#advanced
aria2c --conf-path=aria2.conf --check-config
```

参数错误会指出来源、选项和配置文件行号。出现“已知但未实现”警告时，应以当前构建的 `--help` 和实际运行行为为准；不要仅凭旧版 aria2 配置文件推断本版本支持该功能。
