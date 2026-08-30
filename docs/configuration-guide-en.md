# aria2-rust Configuration Guide

中文版本：[`configuration-guide-cn.md`](configuration-guide-cn.md)

This is the standalone reference for command-line, configuration-file, and RPC options. The option registry is the source of truth for types and validation; the CLI, configuration files, and RPC option handling share these definitions.

## 1. Sources and precedence

Values are applied in this order, from lowest to highest priority: built-in defaults < environment < configuration file < command line. Boolean CLI options accept `--option`, `--option=true`, and `--option=false`. `--no-option` disables an ordinary boolean option.

```text
aria2c --conf-path=aria2.conf https://example.com/file.iso
aria2c --no-conf https://example.com/file.iso
aria2c --check-config --conf-path=aria2.conf
```

Relative paths are resolved from the process working directory. A configuration file contains one `name=value` entry per line. Blank lines and lines beginning with `#` or `;` are ignored. Cumulative options append when repeated; other options use the later value. Known options that are not implemented by the current runtime are retained with a warning and do not change download behavior. Unknown options also produce a warning.

## 2. Types, units, and examples

| Type | Syntax |
| --- | --- |
| Boolean | `true` or `false`; a CLI boolean may omit its value for true |
| Integer | Decimal integer, validated against the option range |
| Size | Bytes, with `K`, `M`, `G`, or `T` suffixes |
| String/Path | Literal string or path; quote CLI values containing spaces |
| Enum | One of the values listed for that option |

Time options use seconds. Rate, cache, and size options use bytes and may use suffixes such as `2M` or `512K`. Do not use the `--` prefix in a configuration file.

## 3. Recommended templates

### Standard download

```ini
dir=downloads
continue=true
check-certificate=true
split=4
max-connection-per-server=4
max-tries=5
timeout=60
```

### Resumable session

```ini
input-file=aria2.session
save-session=aria2.session
save-session-interval=60
keep-unfinished-download-result=true
```

### RPC daemon

```ini
enable-rpc=true
rpc-listen-address=127.0.0.1
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
daemon=true
pid-file=aria2.pid
log=aria2.log
```

## 4. Configuration maintenance

```text
aria2c --conf-path=aria2.conf --check-config
aria2c --conf-path=aria2.conf --repair-config
aria2c --conf-path=aria2.conf --reset-config
```

`--check-config` validates without starting the download engine. `--repair-config` creates a non-overwriting backup and comments out invalid lines. `--reset-config` creates a backup and replaces the file with the built-in default template. All three operations require an explicit `--conf-path`. Do not edit a session file while aria2c is running.

## 5. Complete option catalog

The following catalog covers all canonical names registered by the current `OptionRegistry`, but registered does not mean fully wired. Options explicitly marked `supported: false` are retained for old configuration compatibility; they produce a warning and do not change runtime behavior. For all other options, follow the current build's `aria2c --help` output and observed tests. Compatibility aliases are `max-retries` -> `max-tries`, `enable-lpd` -> `bt-enable-lpd`, `dht-message-path` -> `dht-file-path`, `server-stat-file` -> `server-stat-of`, and `max-downloads` -> `max-concurrent-downloads`.

Options currently marked as unsupported compatibility entries are: `interface`, `multiple-interface`, `async-dns-server`, `dns-timeout`, `enable-async-dns6`, `event-poll`, `optimize-concurrent-downloads`, `optimize-concurrent-downloads-coeffA`, `optimize-concurrent-downloads-coeffB`, `rlimit-nofile`, `select-least-used-host`, `socket-recv-buffer-size`, `dscp`, and `max-http-pipelining`. These names may appear in compatible configuration files, but users should not rely on them to provide the corresponding feature.

### General: basic, session, UI, and logging

| Options | Description |
| --- | --- |
| `dir`, `out` | Download directory and output filename |
| `conf-path`, `no-conf` | Configuration path and configuration-file disable switch |
| `update-check`, `update-check-interval-days` | Update check and interval in days (1..365) |
| `input-file`, `save-session`, `save-session-interval`, `auto-save-interval` | URI/session input and session save intervals |
| `daemon`, `pid-file`, `gid`, `netrc-path` | Daemon mode, PID file, first-task GID, and netrc path |
| `enable-color`, `quiet`, `dry-run`, `human-readable`, `stderr` | Color, quiet mode, check-only mode, readable sizes, and stderr output |
| `download-result`, `keep-unfinished-download-result` | Result display (`default`/`full`/`hide`) and unfinished-result retention |
| `truncate-console-readout`, `show-console-readout` | Console progress display behavior |
| `log`, `log-level`, `console-log-level`, `log-backup-count`, `summary-interval` | Log file, levels, backup count, and summary interval |
| `torrent-file`, `metalink-file`, `show-files` | Torrent/Metalink input and file-list display |
| `metalink-version`, `metalink-language`, `metalink-os`, `metalink-location`, `metalink-base-uri` | Metalink selection and relative-URI base |
| `follow-metalink`, `metalink-preferred-protocol`, `metalink-enable-unique-protocol` | Metalink follow policy (`true`/`false`/`mem`), protocol, and deduplication |

### General: download and network behavior

| Options | Description |
| --- | --- |
| `allow-piece-length-change`, `always-resume`, `check-integrity`, `conditional-get` | Piece length changes, resume, integrity checking, and conditional requests |
| `checksum`, `enable-mmap`, `max-mmap-limit` | Checksum digest, mmap enablement, and maximum mmap file size |
| `deferred-input`, `disable-ipv6`, `hash-check-only`, `parameterized-uri`, `pause`, `pause-metadata` | Deferred input, IPv6, hash-only checking, parameterized URIs, and initial pause |
| `remove-control-file`, `reuse-uri`, `save-not-found`, `force-sequential`, `no-netrc`, `realtime-chunk-checksum` | Control files, URI reuse, not-found results, sequential mode, netrc disable, and live checksum |
| `max-download-result`, `lowest-speed-limit`, `max-file-not-found`, `no-file-allocation-limit` | Result count, minimum speed, not-found retries, and preallocation threshold |
| `uri-selector`, `stream-piece-selector`, `select-least-used-host` | URI, piece, and host selection strategies |
| `optimize-concurrent-downloads`, `optimize-concurrent-downloads-coeffA`, `optimize-concurrent-downloads-coeffB` | Dynamic concurrency and its coefficients |
| `on-download-start`, `on-download-pause`, `on-download-stop`, `on-download-complete`, `on-download-error` | Download lifecycle hooks |
| `stop-with-process`, `startup-idle-time`, `rlimit-nofile` | Process-exit linkage, startup idle time, and file-descriptor limit |
| `async-dns`, `async-dns-server`, `dns-timeout`, `enable-async-dns6` | DNS settings; some compatibility options are unsupported or deprecated |
| `interface`, `multiple-interface`, `event-poll` | Interface binding and event polling; some compatibility options are unsupported |
| `server-stat-timeout`, `server-stat-if`, `server-stat-of` | Server-stat expiry, input, and output files |

### HTTP, FTP, and SFTP

| Options | Description |
| --- | --- |
| `all-proxy`, `http-proxy`, `https-proxy`, `ftp-proxy`, `no-proxy` | Proxies and hosts excluded from proxying |
| `all-proxy-user`, `all-proxy-passwd`, `http-proxy-user`, `http-proxy-passwd`, `https-proxy-user`, `https-proxy-passwd`, `ftp-proxy-user`, `ftp-proxy-passwd` | Proxy credentials |
| `proxy-method`, `user-agent`, `referer`, `header` | Proxy method, user agent, Referer, and custom headers |
| `load-cookies`, `save-cookies`, `http-user`, `http-passwd`, `ftp-user`, `ftp-passwd` | Cookies and HTTP/FTP credentials |
| `connect-timeout`, `timeout`, `max-tries`, `retry-wait` | Connect timeout, total timeout, attempts, and retry wait (seconds) |
| `split`, `min-split-size`, `max-connection-per-server` | Splitting, minimum split size, and per-server connection limit |
| `check-certificate`, `ca-certificate`, `certificate`, `private-key`, `min-tls-version` | TLS verification, CA, client certificate/key, and minimum TLS version |
| `allow-overwrite`, `auto-file-renaming`, `continue`, `remote-time` | Overwrite, automatic renaming, resume, and remote timestamps |
| `enable-http-keep-alive`, `enable-http-pipelining`, `max-http-pipelining`, `http-accept-gzip`, `http-auth-challenge`, `http-no-cache`, `content-disposition-default-utf8`, `use-head`, `no-want-digest-header` | HTTP connection, pipelining, compression, authentication, and response behavior |
| `ftp-pasv`, `ftp-reuse-connection`, `ftp-type`, `ssh-host-key-md` | FTP passive mode, connection reuse, transfer type, and SSH host-key digest |

### BitTorrent (when the bittorrent feature is enabled)

`seed-time`, `seed-ratio`, `bt-max-peers`, `bt-request-peer-speed-limit`, `bt-max-open-files`, `bt-seed-unverified`, `bt-save-metadata`, `bt-force-encryption`, `bt-min-crypto-level`, `bt-detach-seed-only`, `bt-enable-lpd`, `lpd-listen-port`, `bt-enable-web-seed`, `enable-dht`, `dht-listen-port`, `dht-entry-point`, `dht-file-path`, `enable-peer-exchange`, `follow-torrent`, `listen-port`, `bt-prioritize-piece`, `bt-enable-hook-after-hash-check`, `bt-exclude-tracker`, `bt-external-ip`, `bt-hash-check-seed`, `bt-load-saved-metadata`, `bt-lpd-interface`, `bt-metadata-only`, `bt-remove-unselected-file`, `bt-require-crypto`, `bt-stop-timeout`, `bt-tracker`, `bt-tracker-source`, `bt-tracker-update-interval`, `enable-public-trackers`, `bt-tracker-connect-timeout`, `bt-tracker-interval`, `bt-tracker-timeout`, `dht-message-timeout`, `enable-dht6`, `dht-listen-addr6`, `peer-id-prefix`, `peer-agent`, `select-file`, `index-out`, `bt-peer-blocklist`, `enable-utp`, `utp-listen-port`, `bt-keep-alive-interval`, `bt-timeout`, `bt-request-timeout`, `peer-connection-timeout`, `dht-entry-point-host`, `dht-entry-point-port`, `dht-entry-point6`, `dht-entry-point-host6`, `dht-entry-point-port6`, `dht-file-path6`, and `dht-listen-addr`.

These cover seeding, DHT/IPv6 DHT, PEX, LPD/uTP, trackers, peers, file selection, and event hooks. BitTorrent hook options are `on-bt-download-complete` and `on-bt-download-error`.

### Advanced: disk, bandwidth, and process limits

`file-allocation`, `secure-falloc`, `mmap-threshold`, `max-concurrent-downloads`, `max-overall-download-limit`, `max-download-limit`, `max-overall-upload-limit`, `max-upload-limit`, `piece-length`, `disk-cache`, `stop`, `force-save`, `save-server-stat-interval`, `socket-recv-buffer-size`, `dscp`, `max-resume-failure-tries`, `log-max-size`, and `log-max-files`.

Bandwidth, cache, and buffer options use bytes or Size suffixes. `stop` controls stop conditions. `max-download-limit` applies per task; `max-overall-download-limit` applies to the whole process.

### RPC

`enable-rpc`, `rpc-listen-all`, `rpc-listen-port` (1024..65535, default 6800), `rpc-listen-address` (default `127.0.0.1`), `rpc-secret`, `rpc-user`, `rpc-passwd`, `rpc-allow-origin`, `rpc-cors-domain`, `rpc-secure`, `rpc-certificate`, `rpc-private-key`, `rpc-allow-origin-all`, `rpc-max-request-size` (default 2 MiB), and `rpc-save-upload-metadata`.

See [`rpc-guide-en.md`](rpc-guide-en.md) for complete RPC behavior and security configuration.

## 6. Short options

Original-compatible short options are: `-a` allocation, `-c` continue, `-d` dir, `-D` daemon, `-i` input-file, `-j` max-concurrent-downloads, `-k` min-split-size, `-l` log, `-m` max-tries, `-M` metalink-file, `-n` no-netrc, `-o` out, `-O` index-out, `-p` ftp-pasv, `-P` parameterized-uri, `-q` quiet, `-R` remote-time, `-s` split, `-S` show-files, `-t` timeout, `-T` torrent-file, `-u` max-upload-limit, `-U` user-agent, `-V` check-integrity, `-x` max-connection-per-server, and `-Z` force-sequential.

Rust-only aliases include `-B`, `-e`, `-g`, `-G`, `-I`, `-L`, `-r`, and `-X`. `-h`/`--help` and `-v`/`--version` are process actions, not configuration options.

## 7. Validation and troubleshooting

```text
aria2c --help
aria2c --help=#basic
aria2c --help=#http
aria2c --help=#advanced
aria2c --conf-path=aria2.conf --check-config
```

Configuration errors identify the source, option, and configuration-file line. When a known-but-unsupported warning appears, follow the current build's `--help` output and observed behavior; do not infer support from an older aria2 configuration file.
