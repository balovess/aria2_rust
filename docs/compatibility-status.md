# Compatibility status

This file records compatibility evidence for externally observable download
behavior. An unlisted behavior has not been audited here.

## Continue default

- **User-directed difference:** `continue` defaults to `true`, so an existing
  output file can be reused for a resume attempt without explicitly passing
  `-c`. This differs from original aria2's default of `false`; `--no-continue`
  or `continue=false` still disables it.
- **Verification:** core configuration defaults, task-option conversion, CLI
  parsing/application (including `--continue=false` and `--no-continue`), and
  RPC task-option merging (including an explicit false override) are covered by
  focused tests. The full external aria2 CLI/config/RPC differential suite has
  not been run against an original aria2 binary.
- **Session persistence:** a task's explicit `continue=false` is written to the
  session file and restored as false. Omitting it would silently reactivate the
  user-directed true default when the session is resumed. The initial-option
  contract and a focused session round-trip test cover this path.

## HTTP split and piece geometry

- **Supported scope:** `split` is the task-wide HTTP segment budget and has no
  fixed registry maximum. The effective budget is bounded by the payload,
  `min-split-size`, and runtime resource limits.
- **Default alignment:** `min-split-size` defaults to `20M`, matching original
  aria2. `piece-length` defaults to `1M` and defines durable HTTP parent-piece
  boundaries for new downloads. A compatible control-file layout wins on
  resume.
- **User-directed difference:** `max-connection-per-server` defaults to `10`,
  while original aria2 defaults to `1`. It remains the per-download,
  per-authority HTTP/1.1 connection ceiling with the aria2-compatible range
  `1..=16`. `split` also has no registry ceiling but defaults to `16`, while
  original aria2 defaults to `5`.
  With `-x=1`, an HTTP/1.1 source permits one active Range request even when
  `split` is larger. HTTP/2 session pools and multiplexed Range streams use
  their dedicated session and stream limits; `-x` does not cap HTTP/2 sessions.
  Adaptive HTTP Range concurrency target adjustment is disabled by default.
  When explicitly enabled, concurrency starts at the configured effective
  ceiling, backs off on explicit server capacity feedback, then probes upward
  by one Range after a 10-second cooldown and a target-scaled 8..=32 successful
  ranges without an intervening failure. Capacity feedback reduces the target
  by 25%, starts subsequent Ranges at 1-second intervals (doubling to 4 seconds
  on repeated feedback), and uses jittered exponential backoff from 20 seconds
  to 300 seconds. These are Rust extension defaults and can be adjusted per
  download with the `http-adaptive-*` options. The default
  `http-adaptive-enabled=false` fixes each protocol's Range target at its
  configured effective ceiling and disables the adaptive exponential
  capacity cooldown. Explicit `Retry-After` and ordinary `retry-wait` behavior
  remain active.
- **External configuration:** all adaptive controller parameters are available
  through CLI flags, configuration files, and JSON-RPC/XML-RPC option maps.
  `changeGlobalOption` changes the defaults used for future tasks;
  `changeOption` applies immediately to waiting tasks and is queued as pending
  for active tasks, taking effect when they are restarted. `getGlobalOption`
  and task option projections expose the registered values. Individual
  `addUri`/`addTorrent` option maps may set `http-adaptive-enabled=true`
  without changing the process-wide default. These option names are Rust
  extensions and are not part of original aria2.
- **Verification:** focused tests cover the `max-connection-per-server=10`
  default in `ConfigManager` and `FileEntry`, and the `--init` generated
  configuration. CLI help was checked manually. Additional focused tests cover
  values above the old `split=128` ceiling, the `20M` default, configured 4 MiB
  HTTP Range boundaries, the adaptive-disabled default, and reuse of the
  control-file piece layout ahead of a new configuration.

## HTTP Range capacity retries

- **Engine policy:** When adaptive concurrency is enabled, HTTP 429/503
  responses back off exponentially up to 300 seconds by default, honoring
  longer `Retry-After` values when present. The base and cap are configurable
  with `http-adaptive-backoff-base` and `http-adaptive-backoff-max`.
  Consecutive capacity rejection episodes are bounded by `max-retries` even if
  other ranges in the resumed attempt have completed. Successful ranges do not
  clear the backoff streak; it resets after 60 seconds without a 429. When
  adaptive concurrency is disabled, the Range target remains fixed and the
  adaptive exponential cooldown is not applied; ordinary `retry-wait` and
  explicit `Retry-After` behavior remain active. A five-minute no-progress
  limit remains as the final bound for the probe and single-mirror retry
  paths.
- **Configuration:** all `http-adaptive-*` controls are exposed through the
  option registry, CLI, global RPC option updates, and C API global-option
  entry points. RPC `addUri` and C API `add_uri` option maps can override them
  for one task without changing global defaults. Session
  snapshots retain the configured values even when execution normalizes paired
  limits such as recovery successes or pacing intervals.
- **Verification:** an engine-level pause/resume test lets a resumed range
  complete, then rate-limits subsequent ranges and verifies the command exits
  with an error instead of remaining active.

## HTTP mirror circuit-breaker recovery

- **Engine policy:** repeated non-capacity HTTP Range failures temporarily
  disable a mirror for the configured mirror cooldown instead of disabling it
  permanently. A successful in-flight Range clears the circuit breaker, and the
  segment scheduler waits for an expired cooldown rather than reporting that no
  mirror is available while pending work remains.
- **Capacity errors:** HTTP 429/503 remain owned by the adaptive Range
  controller. They trigger its backoff and pacing policy but do not trip the
  independent mirror circuit breaker, avoiding two sequential 60-second waits.
- **Verification:** focused segment-manager tests cover cooldown recovery,
  recovery after a successful in-flight Range, and the exclusion of 429/503
  from the mirror circuit breaker. The concurrent HTTP end-to-end suite was
  run, but its existing capacity-peak assertions failed because the dedicated
  test servers did not observe the expected 429 responses; this fix is not
  claimed as end-to-end acceptance for that separate scheduling behavior.

## HTTP/2 Range session and stream limits

- **Engine policy:** HTTP/2 Range concurrency is bounded by the effective
  split budget and the product of configured session and per-session stream
  limits. The `max-connection-per-server` HTTP/1.1 limit does not define the
  HTTP/2 session count.
- **Verification:** the local HTTP/2 end-to-end test configures one session and
  four streams, confirms concurrent payload ranges use that single negotiated
  session, and exercises slow-range replacement. Core and protocol tests were
  run with BitTorrent, Metalink, and SFTP features enabled.

## Magnet URI parameter decoding

- **Supported scope:** `MagnetLink` parses the `magnet:` URI and extracts
  supported `xt`, `dn`, `tr`, `xl`, `xs`, and `ws` values in the protocol
  crate. Percent escapes are decoded as bytes, and `+` remains a literal plus
  as in original aria2's magnet parser; it is not form-query decoding.
- **UTF-8 boundary:** valid percent-encoded UTF-8 is reconstructed before
  returning Rust `String` values. Invalid decoded UTF-8 is represented with
  replacement characters because the public parser output uses `String`,
  while original aria2's byte strings can retain arbitrary bytes.
- **Observed acceptance differences:** the Rust parser trims surrounding
  whitespace and matches the scheme, parameter names, and `btih`/`btmh` URN
  prefixes without case sensitivity. The local original parser requires the
  exact `magnet:?` prefix and preserves parameter-name case; its v1 parser
  recognizes the exact lowercase `urn:btih:` prefix. These broader Rust
  acceptance rules are retained pending a separate compatibility decision.
- **Verification:** behavior was compared with the local original aria2
  `magnet.cc` and `util.cc` implementations. Protocol magnet tests passed
  (11/11); core magnet-command tests passed (25/25), including `xs=file://`
  reading and HTTP exact-source retrieval; the core URL-decoding integration
  test passed (1/1). No original-versus-Rust runtime differential test has
  been run.

## BitTorrent WebSocket tracker response correlation

- **Protocol behavior:** a WebSocket announce response must identify the
  `announce` action and echo the requested 20-byte `info_hash`; malformed or
  mismatched responses are rejected before tracker statistics or peers are
  accepted. Core retains tracker selection, outbound connection policy, and
  announce lifecycle.
- **Verification:** protocol response-decoding tests cover valid, malformed,
  rejected, and mismatched responses; the core local WebSocket tracker fixture
  echoes the request identity for both started and stopped announces.

## BitTorrent dynamic tracker tiers

- **Extension behavior:** a bencoded HTTP tracker response may include an
  `announce-list`. The protocol parser preserves its URL tier grouping and
  ignores non-string, blank, and empty tier entries. Core appends unseen URLs
  after the configured tiers without replacing or reordering existing torrent
  trackers; an appended tier becomes eligible after earlier tiers fail.
- **Compatibility impact:** this is a Rust extension. Original aria2's
  `DefaultBtAnnounce::processAnnounceResponse` consumes tracker status,
  warning, ID, interval, swarm counts, and peers, but does not consume a
  response-level `announce-list`. This behavior applies to bencoded HTTP
  tracker responses; it does not establish support for such an extension from
  UDP or WebSocket trackers.
- **Verification:** protocol parser tests verify nested tier extraction. A
  core loopback HTTP fixture returns dynamic tiers, rejects the next primary
  announce, then verifies that the newly advertised tracker is selected and
  accepted. No interoperability test against a live tracker has been run.

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
  SHA-256-sess, exercise both CONNECT integrations, and pin an MD5-sess CONNECT
  response vector. Core provider tests cover legacy challenge fields, qop
  calculations, nonce counting, and zeroization.
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
  application are covered by focused tests. CLI parsing, RPC updates and invalid
  values, and C API global-option propagation also have focused coverage. The
  startup `process_options` path and runtime `EngineCommand` path each have a
  loopback behavior test through an accepted TCP connection. CLI, RPC, and C API
  projection tests remain separate; no single test spans those entry points
  through to a live peer. Original aria2 has no corresponding process-wide
  option, so this is a Rust extension.

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
- **Capacity behavior:** when the configured endpoint is below its connection
  limit and a 16-bit ID is free, outbound setup searches the entire ID space
  from a random start before reporting capacity exhaustion. This fixes a
  collision-probability false rejection without changing the limit or wire
  format.
- **Idle cleanup:** established connections now have an independent idle
  deadline, refreshed by valid packets from the peer. This closes unresponsive
  peers even when no packet retransmission timer is active, so the configured
  idle timeout also bounds keepalive-only connections.
- **Verification:** a core test confirms the configured connection cap is
  enforced by the shared transport socket; CLI parsing and RPC option-to-engine
  command propagation have focused tests. A C API FFI regression verifies that
  runtime handshake-timeout and uTP-cap changes produce the corresponding
  engine commands. Protocol unit tests cover ID-space wraparound, the sole
  free-ID case, complete exhaustion, idle retirement without pending
  retransmissions, and preservation of a live connection after an inbound
  connection-ID collision.

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
- **Concurrency:** `bt-tracker-source-concurrency` defaults to 4 and accepts
  `1..=4`, preserving the protocol library's hard safety ceiling while letting
  users lower bursts for multi-source refreshes. Runtime changes apply to
  future refreshes; an in-progress refresh keeps its captured limit. CLI/config,
  C API, and `changeGlobalOption` pass the process setting to `PublicTrackerList`.
- **Configuration ownership:** `bt-tracker-source`,
  `bt-tracker-update-interval`, `bt-tracker-source-concurrency`, and
  `enable-public-trackers` configure the process-shared public tracker catalog
  and are process-engine options.
  `bt-tracker` remains a per-download option for that download's announce
  tiers. Runtime changes to the catalog options are projected to the shared
  catalog through the engine command path.
- **Transport ownership:** `PublicTrackerList` accepts an injected source
  fetcher. The process engine supplies one using its DNS cache, configured
  outbound source-address policy, and TLS verification/identity options;
  standalone protocol users retain the default reqwest transport. The engine
  fetcher applies scheme-specific HTTP/HTTPS proxy options, `all-proxy`, proxy
  credentials, and `no-proxy`; DNS resolution and source binding target the
  proxy endpoint when selected.
- **Announce proxying:** HTTP tracker announces use the same per-download
  scheme-specific proxy options and bind the outbound source to the proxy
  endpoint. WebSocket announce proxying already uses the shared proxy options;
  UDP tracker traffic remains direct UDP and does not use HTTP proxies.
- **Empty source list:** An empty `bt-tracker-source` list is valid through
  startup configuration, the C API, and RPC. Empty list input is represented as
  `List([])` in configuration instead of a one-element list containing an empty
  string. The catalog skips source fetching and retains its current snapshot as
  a fallback.
- **Compatibility impact:** these are Rust extensions; original aria2 does not
  provide public tracker-list source timeout or concurrency options. Defaults
  preserve the prior 15-second timeout and four-fetch concurrency ceiling.
- **Verification:** protocol tests delay an HTTP response body and assert the
  configured deadline aborts the complete fetch; CLI, RPC, and C API tests
  verify empty tracker sources remain an empty list through their configuration
  boundaries. A custom-fetcher protocol test measures the configured concurrent
  source ceiling; CLI, RPC, and C API tests cover propagation and reject values
  above the protocol cap. A core loopback test verifies that the engine fetcher
  reaches the tracker source using the selected local source address. A second
  loopback test verifies HTTP proxy absolute-form requests and Basic proxy
  credentials.
  An announce-specific loopback test verifies the tracker request traverses
  the configured proxy, carries the configured proxy credentials, and binds to
  the selected outbound source address. A second announce test verifies one
  tracker announcer does not reuse a direct client for a later proxied tracker.

## UDP tracker transaction ownership

- **Protocol interface:** `aria2_protocol::bittorrent::tracker::udp_client::UdpTrackerClient`
  owns BEP 15 CONNECT, ANNOUNCE, and SCRAPE transaction state, including
  connection-ID reuse, transaction IDs, source/action validation, and retries.
  It receives a caller-bound `tokio::net::UdpSocket`.
- **Engine policy:** core retains tracker URL resolution through
  `OutboundNetworkPolicy`, binds the socket to the selected address family and
  local source, and maps protocol errors into tracker lifecycle outcomes. UDP
  remains direct UDP and is not routed through HTTP proxy settings.
- **Compatibility impact:** the existing core `UdpTrackerClient` constructors,
  `announce` and `scrape` methods remain as policy adapters. Retry count,
  initial/retry response deadlines, 60-second connection-ID lifetime, response
  validation, and tracker error classification are preserved.
- **Verification:** protocol UDP client loopback tests cover transaction
  matching, unexpected-source rejection, malformed/action-mismatched packets,
  retry deadlines, connection-ID expiry, ANNOUNCE, and SCRAPE. Core tests cover
  source-address/address-family binding and URL-to-policy resolution. External
  tracker interoperability remains unverified.

## BitTorrent DHT process configuration

- **Behavior:** the shared IPv4/IPv6 DHT engines take their listen, bootstrap,
  persistence, and timing settings from the process option snapshot. Per-task
  `enable-dht` and `enable-dht6` still decide whether that task joins each
  family. Explicit bootstrap endpoints are resolved by core before startup;
  absent explicit endpoints, the protocol engine resolves its built-in public
  bootstrap list in the background. This avoids first-task order deciding
  process-wide engine settings. `DownloadEngine::new` uses process defaults;
  library callers supply custom process settings through
  `DownloadEngine::from_process_options`.
- **Resource limits:** process options `dht-max-peer-info-hashes` and
  `dht-max-peers-per-info-hash` configure the Rust DHT engine's local announce
  storage. Defaults remain 4,096 info-hashes and 50 peers per info-hash. The
  original implementation had no corresponding caps; these bounded defaults
  limit memory but can cause older swarm/peer announcements to stop being
  returned under pressure. These settings are available as process
  configuration through CLI/config files and C API session startup, are visible
  through global-option RPC reads and `changeGlobalOption`. Global changes
  immediately update running engines, trim excess entries by least-recently
  updated swarm/peer, and update the process snapshot used by later engines.
  `dht-max-items` independently caps BEP 44 targets at 4,096 per DHT engine;
  when full, the oldest write is evicted. It is exposed through CLI/config,
  C API session options, and global RPC options. Runtime changes trim the
  running store immediately and configure later engine startups.
- **Compatibility reference:** original aria2's `DHTSetup.cc` reads these
  engine settings from `DownloadEngine::getOption()`, while `RequestGroup.cc`
  reads the per-task DHT enable flags. DHT options are not marked with
  `setChangeGlobalOption(true)` in `OptionHandlerFactory.cc`.
- **Runtime contract:** These two Rust-specific resource bounds are available
  to `changeGlobalOption`, but remain excluded from task `changeOption`. Other
  DHT engine settings remain fixed after startup and are excluded from both
  runtime option allowlists. RPC-created and session-restored tasks retain
  per-task enable flags but cannot override shared engine settings.
- **Persistence lifecycle:** DHT startup reads the BEP 44 `.items` snapshot
  through Tokio file I/O and performs bencode decoding and signature checks on
  the blocking pool. The synchronous `DhtItemStore` file API remains available
  to direct library callers.
- **Verification:** focused core tests cover process-option precedence over a
  task override, projection of every configurable DHT timeout/concurrency and
  persistence-age value into the protocol config, process-level peer and
  BEP 44 item-storage
  options through the CLI/C API/config-to-engine path, RPC global-option
  projection, and the runtime allowlist contract. Protocol tests cover routing
  snapshot startup and async item-store file restoration. DHT network
  interoperability under simultaneous tasks with conflicting DHT overrides
  has not been run.

## BitTorrent DHT BEP 44 item storage

- **Behavior:** inbound `put` requires a valid write token; mutable values also
  require a valid signature and monotonic sequence. The store rejects bencoded
  values over 1,000 bytes and mutable salts over 64 bytes. Restored `.items`
  data enforces those same per-item bounds before accepting entries. Each DHT
  engine retains at most 4,096 targets by default. A valid write at capacity is
  accepted and evicts the oldest-written target; lowering `dht-max-items`
  applies the same eviction immediately. This Rust resource policy may make a
  previously stored value unavailable earlier than its BEP 44 expiration time.
  It does not reject the incoming `put` because of the total-store limit.
- **Persistence:** current snapshots serialize targets from oldest to newest,
  so capacity reduction during restore retains the newest entries. Snapshots
  written before stable ordering have no reliable chronology; when capped on
  restore, their persisted list order is used.
- **Compatibility reference:** [BEP 44](https://www.bittorrent.org/beps/bep_0044.html)
  permits nodes to reject values whose bencoded form exceeds 1,000 bytes and
  requires salt strings to be no longer than 64 bytes.
- **Verification:** protocol tests cover remote put validation, capacity
  eviction, runtime limit reduction on a running engine, persisted item order,
  round trips, and rejection of restored entries with oversized values or
  salts. Large-scale network behavior with a saturated store has not been run.

## BitTorrent peer input error classification

- **Behavior:** invalid peer frames, unnegotiated message capabilities, and
  messages outside the active torrent geometry are classified as recoverable
  peer/network failures. The peer actor disconnects that connection; malformed
  remote data is not a local configuration error. TCP and uTP use the same
  classification, and upload-side validation follows it as well.
- **Compatibility reference:** `aria2_original/src/PeerConnection.cc` aborts
  the peer read on invalid framing, while `aria2_original/src/AbstractCommand.cc`
  handles `DlAbortEx` as an abort of the affected command. The Rust code had
  classified validly framed but semantically invalid remote messages as
  `FatalError::Config`; it now follows the peer-failure path.
- **Verification:** a core regression test verifies that an out-of-range
  remote upload request is recoverable, and focused mapping tests cover invalid
  uTP frames and torrent-geometry validation. Full peer interoperability with
  malformed remote implementations has not been run.

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
- CLI conversion now uses a checked `usize` to `i64` conversion, matching other
  numeric options. Values outside the signed registry representation report an
  explicit out-of-range error instead of wrapping before the registry rejects
  them. CLI tests cover a valid value and the 64-bit overflow case.

## BitTorrent Local Peer Discovery parsing

- **Layering:** BEP 14 announcement encoding and BEP 14/legacy parsing live in
  `aria2-protocol::bittorrent::lpd`. Core keeps multicast networking, sender
  address classification, peer registration, and torrent activity lifecycle.
- **Compatibility intent:** the accepted BEP 14 and legacy message formats,
  hash normalization, and port validation remain unchanged.
- **Peer registry behavior:** core retains the existing limit of 50 peers per
  info hash. Repeated announcements for a known peer refresh its port and
  `last_seen` without evicting a different peer; a new peer at capacity replaces
  the oldest entry.
- **Verification:** protocol tests cover valid BEP 14 and legacy messages,
  invalid UTF-8, hash and port rejection; core tests exercise the historical
  parser path and sender attachment, plus bounded-registry refresh and eviction.

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

## Incoming BitTorrent listener ownership

- **Protocol library interface:** `PeerTcpListener` owns binding a specified
  TCP endpoint and accepting a stream with its remote endpoint. The standalone
  `PeerListener` remains a channel-driven helper built on that primitive.
  Neither listener parses handshakes or selects torrent routes.
- **Engine interface:** `BtPeerListenerManager` owns process-wide listener
  lifecycle, info-hash routes, TCP/uTP handshake admission, peer-storage
  admission, and shutdown/draining. It uses `PeerTcpListener` for TCP socket
  binding and accept, while route policy and peer-storage state remain in core.
- **Compatibility impact:** no CLI, RPC, C API, or wire behavior change is
  intended; the engine now consumes the protocol library's TCP listener
  primitive instead of binding Tokio sockets directly.
- **Verification:** protocol loopback tests cover endpoint binding and raw
  accept and standalone-listener shutdown under receiver backpressure; core
  loopback tests cover shared multi-torrent routing, IPv4 peers through IPv6
  registration, route removal, blocked-delivery cancellation, handshake
  limits, accept-error recovery, and socket release on shutdown.
  These tests do not establish interoperability against an upstream aria2
  binary or a remote peer swarm.
- Incoming route registration precedes initial DHT startup. After discovery,
  each route now refreshes its IPv4 and IPv6 DHT handshake capability bits;
  incoming TCP/uTP handshakes choose the bit for the normalized remote address
  family. Private torrents keep both bits disabled. A core loopback regression
  test verifies that the advertised bit changes after route registration.

## BitTorrent WebSeed HTTP exchange

- **Protocol interface:** `aria2_protocol::bittorrent::web_seed::WebSeedHttpClient`
  builds byte-range requests over a caller-configured `reqwest::Client`, accepts
  HTTP 200 and 206, enforces the requested body-size ceiling, and bounds waits
  for response headers and each body chunk. The core reports 404 availability
  changes as soon as response headers arrive and counts received chunks toward
  task network activity.
- **Compatibility impact:** preserves the existing `Range` and `User-Agent`
  headers, the 200/206 acceptance rule, per-read timeout behavior, short-body
  return behavior, oversized-body rejection, and URI fallback policy. No
  CLI/RPC/C API or wire contract change is intended.
- **Verification:** protocol loopback tests cover range/header construction,
  partial and full-content responses, status reporting, and oversized bodies.
  Existing core WebSeed tests cover source-address selection, URI fallback,
  timeout/cancellation, statistics, and task integration. Remote WebSeed
  interoperability remains unverified.

## SFTP connection and transport options

- **Internal URI parsing:** FTP/FTPS and SFTP scheme, authority, userinfo,
  port, and remote-path parsing now live in `aria2-protocol`. Core retains the
  historical FTP anonymous defaults, SFTP URL/netrc/CLI credential resolution,
  FTPS TLS policy, and engine error mapping. Existing focused FTP URI tests and
  protocol SFTP parser tests pass; the SFTP engine URI compatibility tests are
  run with the optional `sftp` feature.
- FTP proxy GET request serialization now lives in
  `aria2-protocol::ftp::proxy`, including the absolute FTP request target,
  Host/Range/cache headers, Basic proxy authentication, and FTP username
  escaping. Core retains proxy selection, socket policy, and response/task
  handling; its public builder delegates to the protocol serializer. Username
  escaping follows RFC 3986 with uppercase hexadecimal and only unreserved
  bytes literal. Verification covers CLI fields through `DownloadOptions`,
  RPC values through the core proxy configuration, C API global options through
  newly created request groups, and the protocol serializer plus core public
  builder output.

- **Extension:** `sftp-connect-timeout` and `sftp-read-timeout` configure the
  SFTP connection-establishment and SFTP I/O read timeouts independently, in
  seconds.
  The connect timeout covers outbound DNS/TCP establishment, SSH negotiation
  and authentication, and opening the SFTP session channel. SFTP packet
  exchanges, including the initial response to INIT, use `sftp-read-timeout`.
  Defaults remain 15 and 30 seconds; each accepts 1 through 600 seconds.
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
  timeout of a pending core connection future. The protocol `connect_with_stream`
  path applies its configured deadline to both SSH handshake and authentication,
  and SFTP channel opening uses that timeout. The initial SFTP packet exchange
  uses the separately configured read timeout. The core deadline includes
  policy-based DNS/TCP and SSH negotiation/authentication. A loopback server
  test verifies that a stalled channel open and stalled SFTP INIT response
  time out. A separate protocol loopback test confirms the server has entered
  password authentication before stalling, then verifies
  `connect_with_stream` returns `ConnectTimeout` within the configured
  connection deadline.
  `cargo check -p aria2-protocol --features sftp` and
  `cargo check -p aria2-core --features "bittorrent metalink sftp"` pass.
  Original aria2 has no matching SFTP-specific timeout options; these are Rust
  extensions.
- **Host-key verification:** the protocol library now enforces the documented
  `Strict`, `AcceptNew`, and `Disable` modes using OpenSSH-format
  `~/.ssh/known_hosts` by default; library callers can supply another file.
  The engine uses `AcceptNew` when `ssh-host-key-md` is absent and pins the
  configured fingerprint when present. Newly accepted engine host keys are
  persisted, and changed keys are rejected without retrying the untrusted
  handshake. The protocol reports trust-policy failures as non-retryable
  configuration errors; transport connect failures remain retryable. This
  strengthens the prior implementation, which accepted every key without
  checking or recording it.
- **Verification:** protocol tests cover unknown-key rejection, first-use
  recording, known-key acceptance, and changed-key rejection. End-to-end
  loopback tests use the local mock SFTP server; focused tests verify host-key
  rejection is terminal even when retries are unlimited. The latest full SFTP
  download run passed 21/21 tests, including authentication, fingerprint
  checks, cancellation, resume, and transfer lifecycle. No public remote SFTP
  server interoperability run has been performed. SSH trust behavior is not
  claimed compatible with original aria2 beyond explicit fingerprint checks.
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
- HTTP and FTP CONNECT callers now supply their established proxy streams to
  `aria2-protocol::http::connect`. The protocol session owns CONNECT framing,
  status handling, informational responses, bounded authentication retry, and
  response-head parsing; callers retain socket and network policy. FTP sends
  credentials only after a 407 challenge, while HTTP retains preemptive Basic
  behavior. Header reads stop exactly after the terminator so tunneled bytes
  coalesced with the final 200 response remain available to FTP. Digest
  computation uses the shared protocol implementation.
- **Verification:** protocol CONNECT tests cover 1xx followed by 200, tunneled
  bytes preserved after the header terminator, challenge-only and preemptive
  Basic authentication, and a fixed MD5-sess response vector. Core HTTP and
  FTP CONNECT loopback tests confirm the FTP greeting remains readable, FTP
  credentials are absent on the first CONNECT and sent only after 407, and
  FTP's policy-selected source address is used.
- **Compatibility intent:** this is an internal relocation. FTP command order,
  FTP control response acceptance, passive-address policy, CONNECT
  authentication timing, retries, and output behavior are intended to remain
  unchanged. Passive negotiation matches original aria2: IPv4 uses PASV, IPv6
  tries EPSV and falls back to PASV after a server response that does not yield
  an EPSV port. EPSV transport/read errors propagate without sending a second
  command on a possibly desynchronized control stream. FTP HTTP CONNECT skips
  informational 1xx responses, matching the existing HTTP CONNECT path.
- **Response bound:** standalone `FtpConnection` and injected
  `FtpControlSession` now share the original aria2 64 KiB maximum FTP response
  size. Bounded reads enforce the cap before appending an oversized line, so a
  peer cannot bypass the limit with one unterminated control line.
- **Verification:** protocol-layer FTP tests (19), core FTP tests (72), FTP
  download end-to-end tests (34), FTP proxy tests (2), and the legacy FTP
  integration tests (13) pass. A mock control stream verifies the injected
  session command sequence; protocol tests cover EPSV response fallback and
  propagation of a closed control stream; a core IPv4 loopback verifies that
  the command is PASV without an EPSV probe. An explicit FTPS mock verifies TLS
  on both control and data channels with certificate checking disabled in that
  local test.
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

## Metalink parsed execution data

- **Ownership:** Metalink XML parsing and file metadata remain in
  `aria2-protocol`. Initial expansion passes each normalized parsed file
  transiently through its in-memory request group to execution, so the command
  adapter does not parse the same XML again for every resource group. Restored
  groups reconstruct the parsed file from persisted source XML and file index.
  Commands retain parsed execution data; request groups retain source bytes for
  session reconstruction.
- **Behavior:** mirror ordering, expected size, hashes, piece hashes, and
  BitTorrent metaurls use the same parser output as before. Parsing errors are
  still surfaced by command construction, before task execution.
- **Verification:** focused Metalink constructor and execution tests cover
  direct mirrors, torrent-metaurl-only inputs, integrity data, grouped files,
  manager-restored source handling, parsed-file handoff, and restored-source
  fallback. The initial-path regression deliberately provides invalid source
  XML alongside a valid cached parsed file. No upstream binary comparison was
  run for this internal ownership change.

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

- **Supported scope:** with `async-dns=true`, DNS uses the operating system's
  resolver configuration and hosts file unless `--async-dns-server` is
  configured. When configured, explicit DNS servers replace the system DNS
  server list, as documented by the
  [aria2 option reference](https://aria2.github.io/manual/en/html/aria2c.html).
  With `async-dns=false`, protocol adapters use system DNS resolution.
- **Compatibility:** this matches the original aria2 `AsyncNameResolver`,
  which calls `ares_set_servers_csv` for a configured list and leaves c-ares'
  system-configured servers in place otherwise ([upstream implementation](https://github.com/aria2/aria2/blob/master/src/AsyncNameResolver.cc)). The original DHT bootstrap command branches on `async-dns` and uses the system resolver when it is false ([DHTEntryPointNameResolveCommand.cc](https://github.com/aria2/aria2/blob/master/src/DHTEntryPointNameResolveCommand.cc)).
- **Fallback:** if Hickory cannot read the system resolver configuration or
  build its resolver, lookup falls back to Tokio's system name lookup. Lookup
  errors also use this fallback only when system DNS is configured. An explicit
  server failure is returned as a name-resolution error and is negative-cached;
  it never silently switches to system DNS. HTTP command assembly propagates
  that error instead of allowing reqwest to retry with system DNS. Each Hickory
  request has a 5-second timeout; Hickory and the fallback share the configured
  30-second default total lookup deadline.
- **Proxy path:** HTTP assembly resolves the proxy as the first hop and lets
  reqwest use those resolved addresses. It does not separately resolve the
  origin when a proxy handles the request. Numeric origin and proxy addresses
  bypass DNS resolution. With `async-dns=false`, direct routing without a
  source constraint leaves resolution to reqwest. A configured source-interface
  policy uses Tokio's system resolver to select compatible addresses for either
  the direct origin or proxy endpoint, without populating or consulting the
  shared Hickory cache.
- **BitTorrent DHT:** with `async-dns=true`, custom bootstrap hostnames use the
  engine's shared DNS cache. If explicit DNS servers are configured, core also
  resolves the default bootstrap list through that cache and passes the
  resulting addresses to the protocol engine; if none resolve for the selected
  address family, DHT starts without public bootstrap instead of retrying
  through system DNS. With `async-dns=false`, core uses the system resolver for
  custom bootstrap hostnames and leaves default bootstrap resolution to the
  protocol engine. The `async-dns-server` list does not replace system DNS on
  this path, matching original aria2.
- **Verification:** on this Windows host, a focused async test starts a local
  UDP DNS fixture, configures it as the explicit server, and resolves a
  synthetic hostname through `DnsCache`. This confirms explicit-server
  runtime wiring. A second fixture returns NXDOMAIN and verifies the error stays
  on the configured-server path rather than falling through to system DNS.
  Task-spawner tests cover HTTP proxy first-hop resolution with async DNS on and
  off, including system lookup for source selection while the shared cache
  remains untouched. A local HTTP proxy fixture confirms reqwest connects to
  the proxy address supplied by the resolver while preserving the origin URI
  in the proxy request. Reading the machine's OS resolver configuration and
  resolving through its configured nameserver are not exercised by these
  fixtures. DHT configuration tests verify numeric custom bootstrap endpoints
  and verify that a failed explicit DNS resolver disables default bootstrap
  without failing the download. A local DNS fixture also verifies that resolved
  default bootstrap addresses reach `DhtEngineConfig`; a live DHT UDP bootstrap
  handshake using the configured DNS server has not been exercised. A core
  fixture also verifies that `async-dns=false` resolves a custom bootstrap
  hostname through system DNS even when the shared cache has a failing explicit
  server configured.

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
  and uTP runtime extensions. RPC and C API now use the same core
  `runtime_commands_for_global_option_changes` projection after their respective
  parsing and configuration commit steps. Focused RPC tests verify command
  updates. C API FFI tests verify projected BitTorrent handshake and uTP option
  commands. C API returns `INTERNAL_ERROR` when the engine command queue
  rejects a projected runtime update instead of reporting success; validated
  option values remain committed, matching RPC's commit-then-dispatch ordering.
  A focused FFI-side test covers a closed command receiver. This does not claim
  end-to-end C ABI interoperability with an external C consumer.
- **Option aliases:** runtime whitelist, feature availability, task changeability,
  and command projection use the registry's canonical option name. For example,
  the accepted `max-downloads` alias updates the `max-concurrent-downloads`
  runtime command instead of being silently ignored.
- **Concurrency range:** `max-concurrent-downloads` accepts `0..=u32::MAX`,
  matching the engine command's `u32` representation and the existing RPC
  conversion boundary. Other entry points reject values above that bound
  instead of wrapping them during startup or runtime updates.
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

## RPC interface

- **External contract:** `/jsonrpc` serves aria2's JSON-RPC 2.0 envelope,
  legacy GET/JSONP form, and WebSocket notifications. `/rpc` serves XML-RPC.
  Original `aria2.*` and `system.*` methods retain their documented parameter
  positions, result field names, wire scalar types, notification payloads,
  authentication behavior, multicall nesting, HTTP status mapping, and error
  messages. JSON-RPC envelope errors still use `-32600`, `-32602`, or
  `-32700`; errors raised while executing a valid method use aria2's code `1`.
- **Wire quirks:** the JSON `jsonrpc` member is ignored, named/object params
  are rejected before dispatch, non-object batch entries are ignored, URI and
  status-key arrays ignore non-string members, JSON torrent/Metalink payloads
  use aria2's permissive Base64 decoder, and XML-RPC string payloads remain raw
  while XML `<base64>` values are decoded. XML multicall error entries expose
  `faultCode` and `faultString`. `changeUri` returns its two counts as JSON
  numbers, matching the original examples.
- **Extensions:** Rust-only methods such as `aria2.getPeerStats`,
  `aria2.getPeerDetails`, `aria2.getTrackers`, `aria2.getDhtStatus`,
  `aria2.saveDhtState`, `aria2.evictDhtNodes`, `aria2.removeDownloadFiles`,
  `aria2.updateBrowserContext`, and `aria2.clearBrowserContext` are additional
  entries in `system.listMethods`. Extra status fields are additive; no
  original field is renamed or removed.
- **Verification:** `cargo test -p aria2-rpc --lib --tests` passes the full
  RPC unit and integration suite, including fixed JSON/XML wire fixtures,
  HTTP/TLS/WebSocket transports, authentication, multicall, lifecycle methods,
  notifications, and concurrent stress coverage. The core-backed regression
  suite verifies the application adapter for the original method surface.
  Upstream source and manual examples were used as the reference. A live
  cross-binary differential run against an original `aria2c` executable remains
  a verification gap because no original binary is available in this workspace.

## BitTorrent and Metalink open-file limit

- **External behavior:** `bt-max-open-files` is a process-wide limit for files
  opened by multi-file BitTorrent and Metalink downloads. Original aria2 owns
  one `OpenedFileCounter` in `RequestGroupMan`, shares it with each
  `MultiDiskAdaptor`, and updates it from `changeGlobalOption`.
- **Core behavior:** `OpenFileBudget` is shared by the engine's BitTorrent and
  Metalink command adapters. Multi-file BitTorrent block and piece writers
  reserve the handles needed for each batch, and resumed/seeding reads reserve
  one handle; Metalink payload streams reserve one output handle. RAII releases
  reservations when those operations close. A runtime decrease applies to new
  reservations while existing batches drain. The process budget is the single
  source of truth, including for tasks created before a global option update.
- **Configuration propagation:** process options initialize the shared budget;
  the common global-option projection emits `SetBtMaxOpenFiles`, used by the
  RPC and C API runtime paths. CLI startup uses the same process-options
  constructor.
- **Verification:** focused core tests cover budget sharing and release,
  resizing/waiter wakeup, multi-file writes split under a smaller process
  budget, startup option initialization, runtime command handling, global
  option projection, and RPC/C API command dispatch. Full original-aria2
  runtime comparison and OS-level descriptor instrumentation have not been run.
