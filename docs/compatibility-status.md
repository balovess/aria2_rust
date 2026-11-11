# Compatibility status

This file records compatibility evidence for externally observable download
behavior. An unlisted behavior has not been audited here.

## HTTP split and piece geometry

- **Supported scope:** `split` is the task-wide HTTP segment budget and has no
  fixed registry maximum. The effective budget is bounded by the payload,
  `min-split-size`, and runtime resource limits.
- **Default alignment:** `min-split-size` defaults to `20M`, matching original
  aria2. `piece-length` defaults to `1M` and defines durable HTTP parent-piece
  boundaries for new downloads. A compatible control-file layout wins on
  resume.
- **Known difference:** `max-connection-per-server` defaults to `1`, matching
  original aria2, and remains the per-download, per-authority HTTP/1.1
  connection ceiling with the aria2-compatible range `1..=16`. `split` also
  has no registry ceiling but defaults to `16`, while original aria2 defaults
  to `5`.
  With `-x=1`, an HTTP/1.1 source permits one active Range request even when
  `split` is larger. HTTP/2 session pools and multiplexed Range streams use
  their dedicated session and stream limits; `-x` does not cap HTTP/2 sessions.
  HTTP Range concurrency starts at the configured effective ceiling, backs off
  on explicit server capacity feedback, then probes upward by one Range after
  a 10-second cooldown and a target-scaled 8..=32 successful ranges without an
  intervening failure. Capacity feedback reduces the target by 25%, starts
  subsequent Ranges at 1-second intervals (doubling to 4 seconds on repeated
  feedback), and uses jittered exponential backoff from 20 seconds to 300
  seconds. These are Rust extension defaults and can be adjusted per download
  with the `http-adaptive-*` options.
- **External configuration:** all adaptive controller parameters are available
  through CLI flags, configuration files, and JSON-RPC/XML-RPC option maps.
  `changeGlobalOption` changes the defaults used for future tasks;
  `changeOption` applies immediately to waiting tasks and is queued as pending
  for active tasks, taking effect when they are restarted. `getGlobalOption`
  and task option projections expose the registered values. These option names
  are Rust extensions and are not part of original aria2.
- **Verification:** focused tests cover values above the old `split=128`
  ceiling, the `20M` default, configured 4 MiB HTTP Range boundaries, and
  reuse of the control-file piece layout ahead of a new configuration.

## HTTP Range capacity retries

- **Engine policy:** HTTP 429/503 responses back off exponentially up to
  300 seconds by default, honoring longer `Retry-After` values when present.
  The base and cap are configurable with `http-adaptive-backoff-base` and
  `http-adaptive-backoff-max`. Consecutive capacity
  rejection episodes are bounded by `max-retries` even if other ranges in the
  resumed attempt have completed. Successful ranges do not clear the backoff
  streak; it resets after 60 seconds without a 429. A five-minute no-progress
  limit remains as a final bound when retries are unlimited.
- **Configuration:** all `http-adaptive-*` controls are exposed through the
  option registry, CLI, global RPC option updates, and C API global-option
  entry points. They configure subsequently created download tasks. Session
  snapshots retain the configured values even when execution normalizes paired
  limits such as recovery successes or pacing intervals.
- **Verification:** an engine-level pause/resume test lets a resumed range
  complete, then rate-limits subsequent ranges and verifies the command exits
  with an error instead of remaining active.

## HTTP/2 Range session and stream limits

- **Engine policy:** HTTP/2 Range concurrency is bounded by the effective
  split budget and the product of configured session and per-session stream
  limits. The `max-connection-per-server` HTTP/1.1 limit does not define the
  HTTP/2 session count.
- **Verification:** the local HTTP/2 end-to-end test configures one session and
  four streams, confirms concurrent payload ranges use that single negotiated
  session, and exercises slow-range replacement. Core and protocol tests were
  run with BitTorrent, Metalink, and SFTP features enabled.

## BitTorrent WebSocket tracker response correlation

- **Protocol behavior:** a WebSocket announce response must identify the
  `announce` action and echo the requested 20-byte `info_hash`; malformed or
  mismatched responses are rejected before tracker statistics or peers are
  accepted. Core retains tracker selection, outbound connection policy, and
  announce lifecycle.
- **Verification:** protocol response-decoding tests cover valid, malformed,
  rejected, and mismatched responses; the core local WebSocket tracker fixture
  echoes the request identity for both started and stopped announces.

## HTTP Content-Range parser

- **Protocol library behavior:** `aria2-protocol::http::ContentRange::parse`
  rejects reversed ranges and ranges whose end is outside a known entity size.
  `ContentRange::size` uses checked subtraction and saturating addition so
  caller-constructed invalid or extreme values cannot underflow. The parser
  retains the existing accepted `bytes=...` and unit-omitted forms used by
  core callers. Unsatisfied `bytes */length` parsing is also provided by the
  protocol crate and reused by HTTP range probing.
- **Compatibility intent:** malformed response ranges are rejected, matching
  the core HTTP response parser's existing bounds checks.
- **Verification:** protocol unit tests cover valid known/unknown totals,
  reversed ranges, end-at-total, and invalid constructed-value sizing.
  Metalink payload length discovery now reuses this parser for satisfied ranges
  and the protocol parser for unsatisfied ranges; a core regression test rejects
  malformed and out-of-bounds values instead of extracting any trailing number.

## HTTP/1.x response header parser ownership

- **Layering:** incremental header framing and parsed response-head types live
  in `aria2-protocol::http::header_processor`. Core retains the historical
  `aria2_core::http::header_processor` entrypoint and maps parser errors to its
  existing `Aria2Error::HttpProtocol` variant.
- **Compatibility intent:** parsing rules, byte-consumption counts, error text,
  and core import paths remain unchanged; parser implementation has one
  protocol-layer owner. Header accumulation remains bounded while body bytes
  following the terminator are discarded without buffering them as headers.
- **Verification:** core header-processor tests exercise the compatibility
  entrypoint; protocol tests cover incremental parsing, body-byte boundaries,
  malformed input, and parser reset.
- Linux splice downloads use the same protocol parser for response syntax while
  retaining the existing 8 KiB input limit, 206 requirement, Content-Length
  checks, and fallback behavior. Focused parser and splice tests validate this
  internal sharing; Linux runtime splice transfer was not exercised on this
  Windows validation host.

- The public core `HttpResponse::from_bytes` compatibility entrypoint now maps
  the protocol response head into its existing core response type and keeps
  body bytes binary-safe. Header parsing inherits the protocol parser's 8 KiB
  bound. Its focused tests cover repeated Set-Cookie headers and binary bodies.
- HTTP/1.1 request framing now has one borrowed-field serializer in
  `aria2-protocol::http::request`; the public core request helper, FTP proxy
  GET, both CONNECT paths, forward-proxy probes, and Linux splice Range requests
  map their existing policy fields into it. Request headers, absolute-form
  targets, body bytes, and ordering are covered by focused tests. The Linux
  splice transfer itself remains unverified on this Windows host.

## Metalink/HTTP header parser ownership

- **Layering:** RFC 5988 `Link` and RFC 3230 `Digest` parsing and result types
  live in `aria2-protocol::http::metalink_http`. Core retains the historical
  module path while the response processor continues to apply mirror-selection
  and checksum policy.
- **Compatibility intent:** parsing and prioritization rules and existing core
  import paths are preserved; only parser ownership changes.
- **Verification:** protocol tests cover individual and combined header
  parsing; a core compatibility-path test confirms the re-export reaches the
  protocol parser.

## Content-Disposition parser ownership

- **Layering:** RFC 6266 parsing, charset decoding, and traversal rejection
  live in `aria2-protocol::http::content_disposition`. Core retains the old
  module path and continues to choose and sanitize final output filenames.
- **Compatibility intent:** strict parsing behavior and UTF-8/legacy charset
  selection remain unchanged; only protocol parser ownership changes.
- **Verification:** the existing Content-Disposition parser suite now runs in
  the protocol crate, and a core compatibility-path test covers the re-export.

## HTTP cookie parsing primitives

- **Layering:** RFC 6265 date parsing/formatting and host/path matching now live
  in `aria2-protocol::http::cookie::parsing`. Request Cookie pairs and
  Set-Cookie field syntax are parsed there too; core retains cookie acceptance
  and domain policy, current-time lookup, storage, expiry behavior, and request
  matching. Retry-After still applies its duration conversion and five-minute
  cap in the HTTP engine while reusing the protocol date parser.
- **Verification:** protocol parser tests cover IMF-fixdate, RFC 850 dates,
  invalid dates, formatting, host/path matching, Cookie pairs, and Set-Cookie
  syntax; 58 core cookie policy tests pass. Cookie persistence tests require a writable
  temporary directory on Windows; two storage tests were denied file access by
  the current sandbox.

## HTTP Digest authentication

- **Layering:** Digest challenge parsing, response computation, and header
  serialization are shared by `aria2-protocol::http::digest`. The protocol
  crate's older `HttpAuth` API, core HTTP types, and public
  `aria2_core::auth::digest_auth` provider delegate syntax and hashing to that
  implementation. Core retains public compatibility values, zeroizing secret
  storage, nonce counting, credential lookup, retry state, and Basic's
  HTTPS-only policy.
- **Supported algorithms and qop:** MD5, MD5-sess, SHA-256, SHA-256-sess,
  SHA-512-256, and SHA-512-256-sess; `auth` and `auth-int` are parsed and
  computed according to the selected challenge. Quoted commas and escaped
  challenge parameters are handled by the shared parser.
- **Verification:** protocol tests cover the RFC 2617 MD5 vector, supported
  challenge syntax and algorithm selection; proxy tests cover `auth-int` with
  SHA-256-sess and exercise both CONNECT integrations. Core provider tests cover
  legacy challenge fields, qop calculations, nonce counting, and zeroization.
  No external authenticated proxy interoperability test was run.

## SOCKS connection handshake ownership

- **Layering:** SOCKS4 and SOCKS5 greeting, username/password negotiation,
  CONNECT framing, and reply parsing now live in `aria2-protocol::socks`.
  Existing core connector types delegate to those functions; core retains
  proxy URL/configuration parsing, connector selection, no-proxy rules, and
  outbound network policy.
- **Compatibility intent:** supported auth methods, address types, request
  bytes, and error messages preserve the prior core connector behavior.
- **Verification:** protocol tests cover SOCKS4 success framing and SOCKS5
  username/password plus IPv4 CONNECT framing; core connector tests exercise
  the compatibility adapters and their existing error paths.

## HTTP live connection status

- **Extension:** task status retains aria2-compatible `connections` and adds
  `http1Connections`, `http2Sessions`, and `http2Streams`. H2 session counts
  represent active client-pool session slots carrying streams; idle pooled
  sessions are excluded. `http2Streams` counts active Range requests.
- **Verification:** RPC status serialization and projection tests cover all
  three extension fields. Runtime H2 pool-slot accounting is covered by the
  HTTP request executor tests.

## Incoming BitTorrent peer handshake timeout

- **Extension:** `bt-incoming-peer-handshake-timeout` is a process-wide setting
  for the maximum wait on each read while receiving an incoming TCP peer
  handshake. It defaults to 30 seconds and accepts 1 through 600 seconds.
  The process-wide scope matches the shared listener, which serves multiple
  torrents before the info-hash route is known. Existing task-scoped
  `peer-connection-timeout` retains its outbound connection meaning.
- **Runtime behavior:** CLI/configuration values are applied when the engine
  starts. `changeGlobalOption` updates the timeout for newly accepted peers;
  handshakes already in progress retain the value captured when accepted.
- **Verification:** protocol-level timeout behavior and core shared-listener
  timeout propagation are covered by focused tests. Original aria2 has no
  corresponding process-wide option, so this is a Rust extension.

## BitTorrent peer connection state

- **Library behavior:** `PeerConnection::state()` reflects remote choke and
  interest messages. Responses to tracked block requests (`Piece` and `Reject`)
  retire their matching request from the connection state. The state is derived
  from protocol messages and does not need a CLI/RPC setting.
- **Verification:** protocol tests cover choke/interest transitions and request
  completion; core peer-connection tests and choking-policy tests pass.

## BitTorrent uTP socket options

- **Settings:** `bt-utp-max-connections` (default 100, range 1–65,536),
  `bt-utp-connect-timeout` (10 seconds), `bt-utp-idle-timeout` (30 seconds),
  and `bt-utp-keepalive-interval` (15 seconds) configure each process-owned
  uTP socket endpoint. Timeout values accept 1–600 seconds.
- **Propagation:** the options are available through CLI, config, C API, and
  `changeGlobalOption`. They apply to newly created uTP endpoints; existing
  endpoints are shared by torrent routes and retain their current socket
  settings.
- **Compatibility impact:** these are Rust extensions; original aria2 does
  not expose the corresponding process-level settings. Defaults match the
  protocol library's existing socket defaults.
- **Verification:** a core test confirms the configured connection cap is
  enforced by the shared transport socket; CLI parsing and RPC option-to-engine
  command propagation have focused tests. C API paths are compile-checked.

## Public BitTorrent tracker source timeout

- **Setting:** `bt-tracker-source-timeout` bounds the complete HTTP fetch for
  each remote public tracker list, including response-body reading. It defaults
  to 15 seconds and accepts 1 through 600 seconds. This is separate from
  `bt-tracker-timeout`, which bounds tracker announces.
- **Propagation:** CLI/config, C API, and `changeGlobalOption` pass the value to
  `PublicTrackerList`. Runtime updates apply to future fetches; an in-progress
  fetch keeps the timeout captured when it started. The public
  `TrackerCatalogConfig` layout remains source-compatible; the protocol library
  exposes separate timeout accessors.
- **Compatibility impact:** this is a Rust extension; original aria2 does not
  provide a public tracker-list source timeout option. The default preserves
  the previous 15-second limit.
- **Verification:** protocol tests delay an HTTP response body and assert the
  configured deadline aborts the complete fetch; CLI and RPC propagation have
  focused tests. C API paths are compile-checked.

## BitTorrent DHT outbound source binding

- **Policy:** when an explicit outbound interface/source address is configured,
  each DHT address-family engine must bind to a matching local source. A family
  with no matching source is skipped; an explicitly configured DHT listen
  address outside the source policy is rejected. It must not fall back to a
  wildcard socket.
- **Compatibility impact:** this closes a route where an IPv4 DHT engine could
  bind `0.0.0.0` under an IPv6-only source policy and send outside the selected
  interface. Direct routing retains the existing wildcard behavior.
- **Verification:** focused configuration tests cover family skipping,
  compatible source selection, and rejection of a conflicting explicit
  address. Full tracker/DHT interoperability under multiple real interfaces
  has not been exercised.

## BitTorrent DHT token rotation interval

- **Behavior:** `dht-token-rotation-interval` now controls the token secret
  rotation check inside the protocol DHT engine. The token tracker retains its
  900-second no-argument default for standalone callers.
- **Compatibility impact:** fixes the engine path, which previously applied an
  additional fixed 900-second gate even when the configured interval was 600
  seconds or a caller selected a shorter interval. Old and new tokens remain
  accepted during the existing grace period.
- **Verification:** protocol token-tracker coverage checks configured intervals
  on both sides of the deadline; engine configuration already maps the option
  into `DhtEngineConfig::token_rotation_interval`.

## BitTorrent DHT bootstrap concurrency

- **Behavior:** bootstrap peer pings respect `dht-max-concurrent-lookups`,
  bounded to the existing 15-ping safety ceiling. A value of zero is clamped
  to one, matching the DHT task executor.
- **Compatibility impact:** lower configured concurrency now limits bootstrap
  fan-out too; the default remains capped at 15 concurrent pings.
- **Verification:** focused task construction coverage checks zero, a lower
  configured value, and a value above the safety ceiling.

## BitTorrent Local Peer Discovery parsing

- **Layering:** BEP 14 announcement encoding and BEP 14/legacy parsing live in
  `aria2-protocol::bittorrent::lpd`. Core keeps multicast networking, sender
  address classification, peer registration, and torrent activity lifecycle.
- **Compatibility intent:** the accepted BEP 14 and legacy message formats,
  hash normalization, and port validation remain unchanged.
- **Verification:** protocol tests cover valid BEP 14 and legacy messages,
  invalid UTF-8, hash and port rejection; core tests exercise the historical
  parser path and sender attachment.

## Incoming BitTorrent handshake admission

- **Behavior:** the process listener allows at most 128 TCP and uTP incoming
  handshake tasks at once. Connections arriving above that process-wide safety
  cap are closed before starting another handshake.
- **Compatibility impact:** admission is unchanged below the cap; an overloaded
  listener now applies a fixed resource bound instead of accumulating handshake
  tasks without limit.
- **Verification:** a local TCP listener test sets the internal cap to one,
  confirms the active handshake remains open, and confirms the excess peer is
  closed promptly. A multi-peer external network stress run has not been done.

## SFTP connection and transport options

- **Internal URI parsing:** FTP/FTPS and SFTP scheme, authority, userinfo,
  port, and remote-path parsing now live in `aria2-protocol`. Core retains the
  historical FTP anonymous defaults, SFTP URL/netrc/CLI credential resolution,
  FTPS TLS policy, and engine error mapping. Existing focused FTP URI tests and
  protocol SFTP parser tests pass; the SFTP engine URI compatibility tests are
  run with the optional `sftp` feature.
- FTP proxy GET username escaping uses the same protocol FTP URI module's RFC
  3986 user-info encoder. Output remains uppercase hexadecimal with only
  unreserved bytes left literal; protocol and FTP proxy builder tests cover
  non-ASCII and reserved characters.

- **Extension:** `sftp-connect-timeout` and `sftp-read-timeout` configure the
  SFTP connection-establishment and SFTP I/O read timeouts independently, in seconds.
  The connect timeout covers outbound DNS/TCP establishment and SSH negotiation
  and authentication. Defaults
  remain 15 and 30 seconds; each accepts 1 through 600 seconds.
  `sftp-compression` enables SSH compression (default `false`),
  `sftp-keepalive-interval` defaults to 60 seconds and accepts 0 through 600
  (`0` disables keepalives), and `sftp-preferred-ciphers` sets the ordered
  comma-separated cipher preference (empty means library defaults). These
  settings are per download and do not change the meanings of aria2's generic
  HTTP/FTP `connect-timeout` and `timeout` options.
- **Propagation:** CLI/configuration values, RPC `addUri` options, and session
  persistence feed the SFTP protocol connection builder through the shared
  `DownloadOptions::from_option_strings` parser. Session restoration no longer
  reparses selected SFTP fields with different validation. `changeOption`
  stores changes for reserved downloads and defers them on active downloads
  until the task is restarted.
- **Verification:** focused CLI, RPC option validation, C API global-option
  propagation, protocol option construction, and session restore tests cover
  the path, including rejection of a persisted zero connect timeout and
  timeout of a pending connection future. Original aria2 has no matching
  SFTP-specific timeout options; these are Rust extensions.
- **Host-key verification:** the protocol library now enforces the documented
  `Strict`, `AcceptNew`, and `Disable` modes using OpenSSH-format
  `~/.ssh/known_hosts` by default; library callers can supply another file.
  The engine uses `AcceptNew` when `ssh-host-key-md` is absent and pins the
  configured fingerprint when present. Newly accepted engine host keys are
  persisted, and changed keys are rejected. This strengthens the prior
  implementation, which accepted every key without checking or recording it.
- **Verification:** protocol tests cover unknown-key rejection, first-use
  recording, known-key acceptance, and changed-key rejection. End-to-end
  loopback tests use the local mock SFTP server; the latest full SFTP download
  run passed 21/21 tests, including authentication, fingerprint checks,
  cancellation, resume, and transfer lifecycle. No public remote SFTP server
  interoperability run has been performed. SSH trust behavior is not claimed
  compatible with original aria2 beyond explicit fingerprint checks.
- **Malformed packet handling:** length-prefixed fields are collected from
  bytes actually present before their contents are allocated, and a directory
  entry count is checked against the minimum possible encoded payload before
  reserving entries. This rejects impossible short packets without limiting
  valid packet sizes. The outer SFTP frame length still has no configured
  maximum; deciding whether to impose one requires a compatibility policy for
  unusually large but valid server responses.

## FTP control-session layering

- **Internal design:** `aria2-protocol::ftp::connection::FtpControlSession`
  wraps a caller-established stream and owns FTP command sequences, command
  framing, RFC 959 response parsing, and path, SIZE, MDTM, PASV/EPSV, and
  EPRT/PORT handling. Core retains address selection and outbound policy, proxy
  setup, data-channel connection/listener policy, task lifecycle, and
  file state.
- FTP-over-HTTP-CONNECT now shares the HTTP tunnel response reader and protocol
  header parser instead of maintaining a separate unbounded line parser. The
  FTP path retains its own outbound-policy connection, authentication, retry,
  and tunnel lifecycle. Informational 1xx responses are skipped consistently
  with the HTTP CONNECT path. Header reads stop exactly after the terminator so
  tunneled bytes coalesced with the final 200 response remain available to FTP.
- **Verification:** HTTP proxy tests and FTP CONNECT loopback tests cover 1xx
  responses followed by 200 in one write and confirm the immediate FTP 220
  greeting remains readable from the returned tunnel stream.
- **Compatibility intent:** this is an internal relocation. FTP command order,
  FTP control response acceptance, passive-address policy, retries, and output
  behavior are intended to remain unchanged. FTP HTTP CONNECT now skips
  informational 1xx responses, matching the existing HTTP CONNECT path.
- **Response bound:** standalone `FtpConnection` and injected
  `FtpControlSession` now share the original aria2 64 KiB maximum FTP response
  size. Bounded reads enforce the cap before appending an oversized line, so a
  peer cannot bypass the limit with one unterminated control line.
- **Verification:** protocol-layer FTP tests (19), core FTP tests (72), FTP
  download end-to-end tests (34), FTP proxy tests (2), and the legacy FTP
  integration tests (13) pass. A mock control stream verifies the injected
  session command sequence; an explicit FTPS mock verifies TLS on both control
  and data channels with certificate checking disabled in that local test.
  Windows test runs set `TEMP` and `TMP` to the writable
  `target/ftp-test-tmp` directory; the default OS temp directory does not allow
  the required control-file replacement in this sandbox. `cargo check -p
  aria2-protocol -p aria2-core`, workspace formatting, and `git diff --check`
  pass. A public remote FTP/FTPS server was not used for interoperability
  testing. Focused tests cover bounded line reads and oversized responses in
  both public FTP client paths.

- **FTPS TLS layering:** `aria2-protocol::ftp::tls::FtpsClient` builds a TLS
  connector once per download attempt and reuses it for the control and data
  channels. Core chooses the explicit/implicit and direct/proxy routes and
  keeps socket/address policy; TLS setup and RFC 4217 protection negotiation
  stay in the protocol crate. Existing FTPS wire order and certificate policy
  are unchanged. Focused local FTPS tests verify protected control and data
  channels; public remote-server interoperability remains unverified.

## Pause and unpause lifecycle status

- **Compatibility:** `active` means the download is currently running;
  `waiting` means it is queued and has not started. Unpausing a group with an
  in-flight command must therefore restore `active`; when its command has
  already stopped, unpause restores `waiting` so the engine can schedule it.
  This follows the [aria2 status definitions and `unpause` behavior](https://aria2.github.io/manual/en/html/aria2c.html#rpc-interface).
- **Implementation:** `RequestGroup::resume()` chooses between these states
  using the in-flight command count. A regression test covers unpause before a
  slow HTTP task stops.
- **Verification:** The slow HTTP unpause-race test, the initial-paused-download
  resume test, and the sequential pause/resume control-file test pass on
  Windows. Tests that atomically replace files must use a writable temporary
  directory; the sandbox system temp path denies `MoveFileExW` replacement.

## DNS resolver defaults

- **Supported scope:** unless `--async-dns-server` is configured, asynchronous
  DNS uses the operating system's resolver configuration and hosts file.
  Explicit DNS server settings continue to take precedence.
- **Compatibility:** this matches the original aria2 `AsyncNameResolver`,
  which initializes c-ares without overriding its system-configured servers.
- **Fallback:** if Hickory cannot read the system resolver configuration or
  build its resolver, lookup falls back to Tokio's system name lookup. Each
  Hickory request has a 5-second timeout; Hickory and the fallback share the
  configured 30-second default total lookup deadline.
- **Verification gap:** the crate compiles with system-config support, but
  runtime resolution against a configured local DNS server has not yet been
  manually checked on Windows.

## C API process-engine option setup

- **Startup mapping:** C API session construction applies the process-owned
  outbound interface policy (`interface` before `multiple-interface`), DNS
  timeout and explicit DNS servers, server-stat freshness and persistence,
  scheduler and global rate limits, plus BitTorrent tracker, uTP, handshake,
  and LPD settings when that feature is enabled. CLI and C API use the same
  `DownloadEngine::from_process_options` core boundary.
- **Verification:** a focused unit test inspects the constructed engine's
  outbound policy, DNS cache, and server-stat configuration with custom values;
  it passes both with and without the BitTorrent feature.
- **Runtime semantics:** global option updates issue commands only for
  settings with explicit runtime handling; other values update the option
  snapshot without rebuilding engine-owned objects. This is consistent with
  original aria2's explicit dispatch model in `RpcMethodImpl.cc` and
  `aria2api.cc`. Rust additionally dispatches its supported tracker, handshake,
  and uTP runtime extensions. A focused startup mapping test does not claim
  runtime reconfiguration or end-to-end C ABI interoperability.
- **C API batch validation:** `changeGlobalOption` validates the entire list
  before mutating configuration, matching upstream `apiGatherChangeableGlobalOption`.
  Invalid values leave earlier valid entries and the `keep-running` extension
  unchanged. It also ignores registered options outside core's shared global
  change policy, such as process-startup `interface`. Focused FFI regression
  tests cover both behaviors.

## HTTP(S) multi-mirror range resume

- **Upstream format:** aria2 documents `.aria2` in-flight piece records as an
  index, piece length, and block bitfield whose bits represent 16 KiB chunks.
  See the [official control-file format](https://aria2.github.io/manual/en/html/technical-notes.html#control-file-aria2-format).
- **Supported scope:** known-length multi-mirror HTTP(S) downloads restore
  in-flight blocks only when the sidecar layout and per-piece geometry match.
  The downloader reuses only the leading contiguous run of complete blocks;
  blocks after a gap are fetched again. The aggregate `completed_length` is
  not treated as a file prefix for a split layout.
- **Durability rule:** output writes are synchronized before in-flight records
  are saved. Completed-piece bits are published only after the corresponding
  output has crossed the same sync barrier.
- **Verification:** engine-level tests pause and resume a multi-mirror download,
  check persisted blocks against the output bytes, force mirror reassignment,
  reject a discontiguous bitmap, and compare the final output byte-for-byte.
- **Known gap:** the upstream `aria2c` executable was not available in the
  validation environment, so cross-version resume with the original binary was
  not exercised. The project parser and writer use the documented native
  in-flight record layout, but this does not establish an end-to-end
  cross-binary interoperability result.

## HTTP body decoder ownership

- **Protocol ownership:** transfer/content coding selection and incremental
  chunked, GZip, Deflate, and BZip2 decoding now live in
  `aria2-protocol::http::stream_filter`. Core public decoder types, selector,
  and `StreamFilter` interface remain compatibility adapters and preserve the
  existing `Aria2Error` mapping.
- **Compatibility:** chunk framing, zlib/raw Deflate fallback, compressed
  formats, streaming state, and decoder error categories are preserved.
  Response filter-chain execution and null-sink behavior remain in core.
  `bzip2-rs` is now a protocol dependency rather than a core dependency.
- **Legacy buffered interface:** `aria2_protocol::http::encoding::{HttpEncoding,
  ChunkedDecoder}` remains available. It has no in-repository production
  callers and retains its independent whole-buffer acceptance and error
  behavior; this compatibility surface is not routed through the incremental
  decoders.
- **Verification:** all 31 focused core stream-filter tests passed, including
  chunk extensions, split input, strict chunk terminators, tolerated bare LF,
  early EOF, and HTTP response decoding. All 29 protocol stream-filter tests
  passed, covering each decoder, header selection, and magic-byte detection.
  Protocol and core clippy with BitTorrent enabled and core clippy with
  BitTorrent, Metalink, and SFTP enabled passed with warnings denied.

## HTTP connect timeout propagation

- **Configuration contract:** `connect-timeout` defaults to 60 seconds and is
  accepted through CLI/config, RPC task/global options, and the C API global
  option entry point. The HTTP engine and Metalink's HTTP client now pass the
  task value to their reqwest clients. Direct, proxy, DNS-pinned,
  TLS-customized, and range-pool HTTP clients are covered; bound-client cache
  keys include the timeout.
- **Correction:** prior to this change, those HTTP clients used a fixed
  15-second timeout even though `DownloadOptions` held the configured value.
  Calls made directly through core with no option retain the 15-second
  HTTP fallback. Metalink keeps its previous 30-second fallback for direct
  core calls with no option. Configured tasks follow the registered 60-second
  default or their explicit value.
- **Verification:** core client-config tests cover configured and fallback
  values; client-pool tests cover timeout-aware reuse and cache separation.
  Metalink tests cover task-option use and its legacy fallback. CLI, RPC option
  parsing/runtime update, and C API global-option tests cover propagation into
  `DownloadOptions`.
