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
  with an error instead of remaining active. The multi-mirror capacity and
  proxy-jitter soak completed once, and its extended eight-iteration soak
  completed twice after the retry scheduling and output-reset fixes. Those
  loopback runs verify exact output bytes and that recovered servers receive
  no additional self-induced capacity rejections. CLI, RPC `addUri`, and C API
  `add_uri` task-option tests now each cover all 13 adaptive controls and
  verify normalization of paired recovery, backoff, and pacing limits.

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

## HTTP mirror failover output integrity

- **Failure mechanism:** concurrent HTTP downloads preallocate the output to
  the advertised entity length. If a URI attempt fails before a trustworthy
  `.aria2` control file exists, a later mirror attempt must not interpret that
  full-length allocation as completed content.
- **Engine policy:** core compares the output length around a failed URI
  attempt. If that attempt changed the output and no control sidecar exists,
  core removes any stale sidecar, truncates and syncs the output, and resets
  task progress before trying another URI. Failures before output mutation
  preserve an existing partial file. When the sidecar exists, the normal
  checkpoint/resume path remains responsible for deciding which bytes are
  durable.
- **Verification:** the adaptive capacity/proxy-jitter soak checks exact
  output bytes and records each requested range/status for failure diagnosis.
  The ordinary soak passed once and the extended soak passed eight repeated
  iterations after this fix. This is local loopback simulation; it does not
  establish behavior against arbitrary remote servers.

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
  recognizes the exact lowercase `urn:btih:` prefix. Rust CLI detection and
  core URI validation require the `magnet:?` delimiter (case-insensitively)
  and reject the malformed `magnet?` form, consistent with the protocol parser.
  The parser's broader whitespace, scheme-case, parameter-case, and URN-case
  acceptance remains documented pending a separate compatibility decision.
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
  announce lifecycle. A peer-close before a response is classified as a
  transport failure; malformed frames and response payloads remain malformed
  response failures.
- **Verification:** protocol response-decoding tests cover valid, malformed,
  rejected, and mismatched responses; the core local WebSocket tracker fixture
  echoes the request identity for both started and stopped announces.
- **TLS configuration:** `wss://` announces use the task's configured CA,
  client identity, certificate-check setting, and minimum TLS version. Core
  builds the TLS connector from `DownloadOptions`; the protocol crate accepts
  that connector and retains WebSocket handshake and message handling. Plain
  `ws://` announces do not load TLS files. Connection establishment uses
  `bt-tracker-connect-timeout`; the WebSocket handshake and announce exchange
  use `bt-tracker-timeout`. Rustls only negotiates TLS 1.2 or newer, so a
  configured TLS 1.1 minimum is enforced as TLS 1.2.
- **Verification:** a local WSS tracker test verifies task-specific CA
  propagation. Public tracker interoperability, mutual-TLS identity selection,
  and TLS-version negotiation have not been externally exercised. A local
  plain-WebSocket fixture verifies that unused TLS file paths are not loaded.

## BitTorrent IPv6 tracker peer list compatibility

- **Behavior:** HTTP tracker `peers6` accepts BEP 7 compact IPv6 entries and
  dictionary-list peer entries. Compact remains the standard wire format; list
  support preserves the behavior of the original `extractPeer` helper, which
  handles peer dictionaries for either address family.
- **Verification:** protocol response tests cover both compact and
  dictionary-list IPv6 peers. The compatibility reference is
  `aria2_original/src/bittorrent_helper.h::extractPeer`.

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
- HTTP proxy response status classification, Basic/Digest challenge selection,
  and timeout-bound response-head reads now use protocol-owned implementations.
  The response-head transport reader consumes exactly the header terminator,
  preserving following response-body bytes for the public
  `forward_get_with_auth` stream. Core retains proxy TCP connections,
  reconnects, and the public `HttpProxyForward`/`ProxyResponse` interfaces.
- **Verification:** 241 HTTP protocol tests and 33 core proxy tests pass. The
  core loopback cases cover 407 authentication/reconnection, the HEAD probe's
  fresh-stream contract, and reading a response body after the parsed headers.
  No external authenticated-proxy interoperability test was run.

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

- **Layering:** Authentication scheme-token parsing plus Digest challenge
  parsing, response computation, and header serialization live in
  `aria2-protocol::http`. Core preserves its public `AuthScheme` and challenge
  result types by mapping protocol values, and retains credential lookup,
  nonce/retry state, and request policy. The protocol
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

## HTTP redirect credential scope

- **User-confirmed behavior:** after any redirect changes scheme, host, or effective
  port, core removes origin `Authorization`, cookies supplied as custom
  headers, and conventional sensitive custom headers (auth, token, key,
  password, secret, session, signature, and CSRF names) for the rest of that
  redirect chain. Returning to the original origin does not restore them.
  Cookie-jar cookies are selected for each current URL, and proxy credentials
  remain governed by the configured proxy route and proxy challenge. Activated
  Basic credentials are carried from metadata GET through Range probing,
  concurrent segments, and sequential retries, scoped by host, port, and path,
  and still gated by the redirect origin scope.
- **Protocol library:** the same origin-scope and sensitive-header primitives
  now live in `aria2-protocol::http::redirect`. The standalone public
  `HttpClient` uses a bounded manual redirect loop so conventional custom
  secrets are removed on cross-origin hops; core keeps its own redirect loop
  because it must coordinate task auth, cookies, metadata, ranges, and URI
  lifecycle.
- **Reference comparison:** the local `aria2_original` source creates each
  redirected `HttpRequest` through `AuthConfigFactory::createAuthConfig` and
  resolves configured HTTP credentials for the request host. This supports
  source-level evidence that forwarding may differ; this implementation
  intentionally follows the selected safer cross-origin policy. No live
  original-binary redirect capture was performed.
- **Verification:** core loopback coverage verifies same-origin challenge
  retry, then a cross-origin redirect where the destination receives no
  origin Authorization or sensitive custom headers while a non-sensitive
  trace header remains. In-memory metadata downloads also follow redirects
  with the same origin filtering. A range-probe loopback case now also covers
  a redirect returned by the authenticated retry and verifies that the target
  receives neither source credentials nor sensitive custom headers. Protocol
  core Range auth coverage also proves an activated Basic credential is reused
  by a second Range downloader through their shared task cache. Metadata-GET
  loopback tests confirm activated challenge credentials are preemptively sent
  on the following Range probe and on the sequential GET when the prepared
  response must be discarded. Sequential retries and concurrent-to-gap
  fallback also retain the task cache. A loopback gap test verifies that the
  activated source credential is sent to the source and stripped after a
  cross-origin redirect while non-sensitive tracing headers remain. A
  sequential transfer test interrupts the authenticated response body and
  verifies the retry reuses the activated Basic credential.
  loopback coverage uses different ports to prove the standalone client
  removes Authorization, Cookie, and `X-API-Key` while preserving a trace
  header; it also checks POST-to-GET
  conversion and redirect-limit enforcement. Request-policy coverage pins
  origin comparison, irreversible stripping after crossing and returning, and
  independent proxy challenge credentials. HTTPS-to-HTTP downgrade is covered
  by the shared scheme-aware origin comparison, not a live TLS integration
  fixture.

## HTTP authentication diagnostics

- Core auth configuration, cached Basic credentials, `.netrc` storage/parser
  values, and protocol `AuthCredentials` now redact usernames and passwords in
  `Debug` output. `AuthConfig::auth_text()` and its explicit `Display`
  implementation retain their existing credential-returning behavior.
- Core Basic/Digest and `.netrc` resolution diagnostics no longer print
  usernames. Malformed Digest challenges preserve their source status: proxy
  challenges report 407, while origin challenges report 401.
- HTTP proxy configuration warnings now log only the parsed proxy scheme,
  host, and port; malformed URLs use a fixed label. Reqwest errors have their
  URL removed before logging, so embedded proxy credentials are not emitted.
- Raw HTTP header log filtering now identifies confidential fields by parsed
  header name rather than fixed byte offsets. This handles optional whitespace
  after the colon and non-ASCII neighboring lines without panicking. It
  continues to redact Authorization, Proxy-Authorization, Cookie, and
  Set-Cookie values.
- These changes affect diagnostics only; request headers and authentication
  wire behavior is unchanged. Focused core and protocol tests cover proxy
  label redaction and challenge status classification.
- CLI parsing, RPC global/task option merging, and C API global/task option
  merging each have focused coverage showing that HTTP credentials, challenge
  mode, `no-netrc`, and custom Authorization headers reach the created task's
  `DownloadOptions`.

## SOCKS connection handshake ownership

- **Layering:** SOCKS4 and SOCKS5 greeting, username/password negotiation,
  CONNECT framing, and reply parsing are available as synchronous reusable
  primitives in `aria2-protocol::socks`. The async HTTP downloader uses
  reqwest's SOCKS connector so proxying shares its HTTP client, connection
  pool, and request lifecycle. Protocol owns reusable synchronous SOCKS
  connectors, proxy URL parsing, and the reusable `NoProxyMatcher` for exact
  host/IP and domain/CIDR matching. Core retains proxy precedence, per-task
  `no-proxy` application, DNS pinning, and outbound address policy; the reqwest
  proxy callback now uses that same matcher for actual request routing. This
  avoids reqwest's broader suffix interpretation of unprefixed host entries,
  which could disagree with core's first-hop address and source selection. The
  public core module re-exports protocol APIs at their historical paths. The
  standalone `ProxyManager` retains its legacy single-proxy behavior across URL
  schemes; active download requests continue to use core's scheme-specific
  proxy selection.
- **Verification:** protocol tests cover case-insensitive domain matching,
  IP/CIDR matching, invalid CIDR prefix rejection, and newline-separated
  entries. Newline support preserves typed list values through RPC's list
  serialization. Core tests exercise the historical `NoProxyMatcher` path for
  domains, IPs, and wildcard matching. These helper tests do not exercise
  external proxy servers.
- **Compatibility intent:** HTTP and HTTPS proxy transports and credential
  handling remain unchanged. Unprefixed `no-proxy` host entries now follow
  upstream exact-host matching instead of reqwest's broader suffix behavior;
  this restores consistency with core's first-hop selection. SOCKS URLs remain
  an additive extension to the upstream manual's documented HTTP proxy URL
  format for `--all-proxy`. Valid SOCKS exchanges remain unchanged; malformed
  response fields and username/password inputs that cannot be represented by
  the wire format now fail before a tunnel is returned or credentials sent.
- **`no-proxy` comparison:** upstream `inNoProxy` uses exact matching for an
  unprefixed hostname and suffix matching for a leading-dot entry; its CIDR
  comparison applies to numeric URI hosts without DNS resolution. The shared
  Rust matcher likewise uses exact matching for unprefixed names and applies
  CIDR only to IP literals. It additionally accepts `*` and `*.domain`, and
  treats a leading-dot suffix as including the apex domain; those are recorded
  extensions. Request routing, first-hop selection, and DNS/source policy now
  all use the shared matcher instead of mixing it with reqwest's default
  suffix matcher.
- **Verification:** a core loopback test uses separate target and proxy
  listeners to verify that an unprefixed `example.test` exclusion does not
  bypass the proxy for `download.example.test`, matching the first-hop
  selector. FTP coverage verifies exact-host selection and performs a real
  local direct transfer while an HTTP proxy is configured. A WebSocket tracker
  loopback test confirms a matching `no-proxy` rule uses the direct socket.
  CLI list parsing, RPC global and per-task list propagation, and C API global
  option propagation are covered. These checks do not exercise external proxy
  servers.
- **Verification:** protocol tests cover SOCKS4 success framing and SOCKS5
  username/password plus IPv4 CONNECT framing; core adapter tests cover their
  error paths. SOCKS4 rejects replies whose response version is not zero, and
  SOCKS5 rejects a successful CONNECT reply whose reserved byte is nonzero,
  as required by their wire formats. SOCKS4 rejects embedded NUL in user IDs;
  SOCKS5 rejects partial, empty, and overlong username/password inputs before
  writing the handshake. It also rejects a server-selected authentication
  method that the client did not offer, rather than sending empty fallback
  credentials. A live local SOCKS5 fixture verifies `all-proxy` routing,
  resolved proxy address pinning, username/password authentication, and an HTTP
  response carried through the SOCKS tunnel. External SOCKS proxy
  interoperability remains unverified. CLI global, RPC global/per-task, and
  C API global option tests verify that SOCKS `all-proxy` values reach the
  resulting `DownloadOptions`.
- **Protocol references:** [RFC 1928, Section 6](https://www.rfc-editor.org/rfc/rfc1928.html#section-6)
  requires the SOCKS5 reply's reserved field to be zero; [RFC 1929](https://www.rfc-editor.org/rfc/rfc1929.html)
  requires username and password lengths of 1 through 255 bytes. The SOCKS4
  reply version is zero in the [SOCKS4 specification](https://datatracker.ietf.org/doc/draft-vance-socks-v4/05/).
- **Proxy URL userinfo:** `ProxyUrl` percent-decodes username/password
  components and matches schemes case-insensitively; `ProxyConfig::to_url`
  percent-encodes credentials, preserves a nonempty username without a
  password, and brackets IPv6 proxy hosts. For valid UTF-8 userinfo, this
  matches the local original aria2 URI parser and constructor; protocol and
  core proxy-config tests cover encoded delimiters, username-only userinfo, an
  IPv6 authority, and `@` characters in ignored path/query/fragment components.
  Parsing uses the final authority `@` delimiter so literal `@` in a username
  remains compatible with the original parser. It rejects empty userinfo,
  malformed bracketed hosts, and unbracketed IPv6. An HTTP proxy URL without
  an explicit port defaults to 80, matching `aria2_original`'s HTTP default;
  SOCKS defaults to 1080 and HTTPS to 443. Protocol and core tests cover these
  authority boundaries and defaults.
  HTTP and FTP core configuration share the decoded parser result;
  a local HTTP proxy regression verifies the actual Basic authorization header
  for escaped username/password delimiters, and an FTP configuration test
  verifies mixed-case schemes and decoded credentials. The public credential
  fields are
  `String`, so percent-encoded invalid UTF-8 octets decode lossily and are not
  byte-for-byte compatible. No external proxy requiring escaped credentials
  was contacted. Invalid proxy URL errors omit raw userinfo; core HTTP and FTP
  error-path tests verify that configured credentials do not appear in
  returned diagnostics.

## Proxy selection and credential scope

- **Behavior:** Core selects a nonempty, parseable protocol-specific proxy URL
  first and falls back to `all-proxy` if that URL is absent or invalid. It
  resolves credentials from the same selected URL: matching protocol-specific
  user/password options override corresponding URL userinfo fields, while
  `all-proxy-user/passwd` apply only when `all-proxy` is selected. This avoids
  sending credentials intended for the global proxy to a different
  protocol-specific proxy.
- **Compatibility reference:** `aria2_original/src/AbstractCommand.cc`
  constructs the protocol-specific proxy URI first and uses `all-proxy` only
  when that URI is empty. `makeProxyUri` applies the selected option pair to
  the selected URI; it does not merge credentials from the other proxy scope.
- **Verification:** core option tests cover per-protocol credentials, embedded
  URL fallback, invalid-specific-URL fallback, and isolation between specific
  and global credentials. HTTP, FTP, tracker, WebSeed, and metadata-source
  assembly call the same core credential resolver. CLI and C API tests cover
  FTP proxy credential propagation; RPC tests cover global and task-level proxy
  URL and credential field propagation. Mixed credential-scope resolution is
  verified in core option tests, but no single test spans an entry point through
  a live proxy authentication exchange.

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
  for the maximum wait on each required TCP read or uTP transport receive while
  receiving an incoming peer handshake. It defaults to 30 seconds and accepts
  1 through 600 seconds. This matches the existing TCP protocol helper's
  per-read timeout semantics; the total wall-clock time can exceed the setting
  when a handshake requires multiple reads.
  The process-wide scope matches the shared listener, which serves multiple
  torrents before the info-hash route is known. Existing task-scoped
  `peer-connection-timeout` retains its outbound connection meaning.
- **Runtime behavior:** CLI/configuration values are applied when the engine
  starts. `changeGlobalOption` updates the timeout for newly accepted TCP and
  uTP peers; handshakes already in progress retain the value captured when
  accepted.
- **Verification:** protocol-level timeout behavior and core shared-listener
  application are covered by focused tests. CLI parsing, RPC updates and invalid
  values, and C API global-option propagation also have focused coverage. The
  startup `process_options` path and runtime `EngineCommand` path each have a
  loopback behavior test through an accepted TCP connection; a core test also
  verifies the configured timeout closes an incomplete incoming uTP handshake.
  CLI, RPC, and C API projection tests remain separate; no single test spans
  those entry points through to a live peer. Original aria2 has no corresponding
  process-wide option, so this is a Rust extension.

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
- **Verification:** a process-options test creates a shared transport after
  startup and confirms that its configured connection cap is enforced; CLI
  parsing and RPC option-to-engine command propagation have focused tests. A
  C API FFI regression verifies that runtime handshake-timeout changes and all
  four uTP socket settings produce the corresponding engine commands. Protocol
  unit tests cover ID-space wraparound, the sole
  free-ID case, complete exhaustion, idle retirement without pending
  retransmissions, idle-deadline refresh on valid peer traffic, the fact that
  local keepalive sends do not refresh that deadline, and preservation of a
  live connection after an inbound connection-ID collision.

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
  connection-ID reuse, transaction IDs, source/action validation, retries, and
  stable association of out-of-order responses with concurrent requests.
  It receives a caller-bound `tokio::net::UdpSocket`.
- **Response association:** a structurally valid SCRAPE payload is accepted
  only when it contains exactly one result for each requested info hash;
  truncated or excess result sets are reported as malformed responses instead
  of returning values that cannot be associated with the request hashes.
- **ANNOUNCE peer family:** compact peers are decoded using the IP family of
  the tracker's UDP response source (6-byte IPv4 entries or 18-byte IPv6
  entries), as specified by BEP 15. The legacy protocol parser helper remains
  IPv4-oriented for callers that use it directly.
- **SCRAPE request sizing:** the client sends at most 74 info hashes per UDP
  SCRAPE request and splits larger library requests into ordered batches. The
  returned results retain the input order. This follows BEP 15's approximate
  packet-size guidance and prevents large public API inputs from producing
  unusually large, fragmentation-prone UDP datagrams.
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
  retry deadlines, connection-ID expiry, ANNOUNCE, SCRAPE, and rejection of
  SCRAPE result counts that differ from the requested hash count. A loopback
  test sends 75 hashes and verifies ordered results across 74-hash and 1-hash
  requests, and a send-failure test verifies retries retain their hash
  association. Concurrent response and timeout removal coverage verifies that
  remaining transaction indices are updated. Parser tests cover IPv4 and IPv6
  compact peer strides, including an IPv6 tracker-source response. Core tests
  cover source-address/address-family binding and URL-to-policy resolution.
  External tracker interoperability remains unverified.

## BitTorrent tracker announce cancellation

- **Lifecycle behavior:** cancelling a tracker announce during startup stop or
  when the total stopped-announce budget expires clears the core announce
  counter and per-URL in-flight state. The shared `aria2.getTrackers` snapshot
  is republished after cleanup; cancellation does not count as a tracker
  failure or advance the failover cursor.
- **Ownership:** HTTP, UDP, and WebSocket protocol transports own their
  respective exchanges and deadlines. Core owns cancellation of a task-level
  announce, clears queued UDP transactions before reusing that client, and
  reconciles the torrent-wide tracker snapshot.
- **Verification:** a slow HTTP stopped-announce fixture checks the total
  shutdown deadline and verifies the snapshot has no in-flight tracker after
  cancellation. A UDP loopback fixture cancels a pending start transaction and
  verifies the subsequent stopped event is the only announce sent. A protocol
  client test verifies cancellation clears every transaction queue while
  retaining valid connection IDs. A tracker-actor fixture verifies stop remains
  serviceable while initial public announces are pending. External tracker
  interoperability is not relevant to this local cancellation-state contract.

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
  Both DHT storage-limit updates use the engine's bounded control queue, so
  they are not delayed behind queued download submissions.
  `dht-max-items` independently caps BEP 44 targets at 4,096 per DHT engine;
  when full, the oldest write is evicted. It is exposed through CLI/config,
  C API session options, and global RPC options. Runtime changes trim the
  running store immediately and configure later engine startups.
- **Compatibility reference:** original aria2's `DHTSetup.cc` reads these
  engine settings from `DownloadEngine::getOption()`, while `RequestGroup.cc`
  reads the per-task DHT enable flags. DHT options are not marked with
  `setChangeGlobalOption(true)` in `OptionHandlerFactory.cc`.
- **Runtime contract:** These Rust-specific resource bounds are available
  to `changeGlobalOption`, but remain excluded from task `changeOption`. Other
  DHT engine settings remain fixed after startup and are excluded from both
  runtime option allowlists. RPC-created and session-restored tasks retain
  per-task enable flags but cannot override shared engine settings.
- **Persistence lifecycle:** DHT startup reads the BEP 44 `.items` snapshot
  through Tokio file I/O and performs bencode decoding and signature checks on
  the blocking pool. The synchronous `DhtItemStore` file API remains available
  to direct library callers.
- **Nested command shutdown:** after handing DHT engines to the payload
  `BtDownloadCommand`, magnet execution awaits that command's shutdown on both
  success and failure. Locally owned engines finish asynchronous persistence;
  process-shared engines remain running. A successful zero-length payload
  regression verifies the handed-off local engine reaches shutdown and writes
  its DHT state file. At final engine shutdown, the registry also drains
  per-GID DHT handles left by commands that exceeded the graceful-shutdown
  deadline; shared handles are deduplicated before asynchronous shutdown. A
  registry regression covers draining a shared engine retained in both maps.
- **Verification:** focused core tests cover process-option precedence over a
  task override, projection of every configurable DHT timeout/concurrency and
  persistence-age value into the protocol config, process-level peer and
  BEP 44 item-storage
  options through the CLI/C API/config-to-engine path, RPC global-option
  projection, and the runtime allowlist contract. Current focused runs passed
  59 core DHT tests, 12 protocol peer-storage tests, and 6 CLI/RPC propagation
  tests. Protocol tests cover routing snapshot startup and async item-store
  file restoration. DHT network interoperability under simultaneous tasks
  with conflicting DHT overrides has not been run.

## BitTorrent DHT BEP 44 item storage

- **Behavior:** inbound `put` requires a valid write token; mutable values also
  require a valid signature and monotonic sequence. CAS is checked against an
  existing value and ignored if no value exists, as BEP 44 specifies. The store
  rejects bencoded values over 1,000 bytes and mutable salts over 64 bytes.
  Restored `.items` data enforces those same per-item bounds before accepting
  entries. Each DHT
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
  salts. Handler regression tests cover malformed `cas`/`seq` types and `nodes`
  plus `nodes6` output; mutable update coverage checks CAS when no value exists.
  Large-scale network behavior with a saturated store has not been run.

## BitTorrent DHT routing response node quality

- **Behavior:** local iterative lookups may use unverified restored/discovered
  nodes as candidates to probe. `find_node`, `get_peers`, BEP 44 `get`, and
  BEP 51 `sample_infohashes` responses now advertise only recently verified
  good nodes, as required for routing-table responses by BEP 5. A bucket's
  replacement cache stores a node ID at most once and will not cache a node
  that is already active in that bucket. Bucket refresh targets preserve only
  fixed prefix bits; remaining bits, including the unaligned boundary byte,
  are randomized. An unverified duplicate cannot replace a verified endpoint,
  and rejected candidates do not reset the bucket refresh age.
- **Compatibility impact:** peers no longer receive contacts that this node
  has not recently verified. Local lookup seeding and persistence restoration
  continue to retain unverified candidates until they answer a valid query.
- **Verification:** routing-table tests distinguish lookup candidates from
  advertisable good nodes and exercise repeated candidate insertion; bucket
  tests cover non-byte-aligned refresh prefixes and verified-endpoint
  preservation; handler and loopback tests cover node response construction.
  External DHT interoperability has not been exercised.
- **Compatibility reference:** [BEP 5](https://www.bittorrent.org/beps/bep_0005.html)
  defines routing-table responses in terms of the closest good nodes.

## BitTorrent peer input error classification

- **Behavior:** invalid peer frames, unnegotiated message capabilities, and
  messages outside the active torrent geometry are classified as recoverable
  peer/network failures. The peer actor disconnects that connection; malformed
  remote data is not a local configuration error. TCP and uTP use the same
  classification, and upload-side validation follows it as well. Disconnects
  do not by themselves add an address to the temporary bad-peer list. That
  list is reserved for a completed piece whose hash fails and whose contributing
  peer is unambiguous, matching the upstream call site; ordinary transport
  closure and malformed frames therefore remain eligible for later retry.
- **Compatibility reference:** `aria2_original/src/PeerConnection.cc` aborts
  the peer read on invalid framing, while `aria2_original/src/AbstractCommand.cc`
  handles `DlAbortEx` as an abort of the affected command. The Rust code had
  classified validly framed but semantically invalid remote messages as
  `FatalError::Config`; it now follows the peer-failure path. Upstream adds a
  bad peer only from `aria2_original/src/BtPieceMessage.cc` after a completed
  piece fails hash verification.
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

## BitTorrent DHT announce token binding and secret strength

- **Behavior:** announce tokens are bound to the requesting IP address, info
  hash, and a process-local random 256-bit secret. A change in the requester's
  UDP source port does not invalidate a token; a different source IP does.
  `TokenTracker::with_secret([u8; 4])` remains available for deterministic
  callers, and `with_secret_bytes([u8; 32])` supports explicit full-width
  secrets.
- **Compatibility impact:** this follows BEP 5's IP-based token check and
  permits NAT/source-port changes between `get_peers` and `announce_peer`.
  The token remains opaque on the wire, with its existing 40-character
  representation. Tokens generated before a process restart are invalidated
  when the process-local secret is regenerated.
- **Verification:** focused protocol tests cover port changes, IP changes,
  deterministic secrets, and full-width secrets. Interoperability against an
  external DHT implementation has not been exercised.

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
  CLI/RPC/C API or wire contract change is intended. The task's configured
  `user-agent` now reaches live WebSeed range requests, with an explicit
  `User-Agent` entry in `header` retaining precedence; standalone WebSeed
  clients retain the library default. Task `header` values and resolved
  preemptive HTTP Basic credentials now reach live WebSeed requests. Redirects
  are handled per hop; origin Authorization, Cookie headers supplied through
  `header`, and conventional sensitive custom headers are stripped permanently
  after an origin change.
  Proxy authentication remains attached to the configured proxy route.
  HTTP/HTTPS/all proxy selection, credentials, and `no-proxy` reach live
  WebSeed clients through the task's `DownloadOptions`. Cookie-jar integration
  and HTTP challenge retries are not yet wired into WebSeed requests.
- **Verification:** protocol loopback tests cover range/header construction,
  partial and full-content responses, status reporting, oversized bodies, and
  cross-origin removal of Authorization, Cookie, and sensitive custom headers
  while preserving public tracing headers.
  A core loopback proxy fixture verifies `all-proxy`, Basic proxy credentials,
  and outbound source binding with an unresolvable origin hostname. Existing
  core WebSeed tests cover source-address selection, URI fallback,
  timeout/cancellation, statistics, and task integration. The local
  `aria2_original` source defines `all-proxy` as applying to all protocols and
  `http-proxy` to all HTTP downloads, consistent with the aria2 1.37.0 manual.
  No original-binary differential or remote WebSeed interoperability run has
  been performed.

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
  builder output. The engine uses the new fallible protocol builder; the
  existing infallible `build_get_request` remains available for compatibility
  and serializes its supplied fields unchanged. FTP proxy GET and CONNECT
  requests reject `User-Agent` values containing CR, LF, DEL, or other
  forbidden control bytes before writing the request, preventing configured
  values from injecting additional fields. HTAB remains allowed by HTTP
  field-value syntax. This is an intentional compatibility hardening for
  malformed header values.

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
- **Propagation:** CLI/configuration values, RPC `addUri` options, C API
  global and per-task values, and session persistence feed the SFTP protocol
  connection builder through the shared
  `DownloadOptions::from_option_strings` parser. Session restoration no longer
  reparses selected SFTP fields with different validation. `changeOption`
  stores changes for reserved downloads and defers them on active downloads
  until the task is restarted. RPC task creation converts list-valued global
  `OptionValue`s to the newline-separated string form accepted by the shared
  registry before merging them with per-task RPC options. This keeps global
  list settings such as `sftp-preferred-ciphers` valid through task creation.
- **Verification:** focused CLI, RPC option validation, C API global and
  task-option propagation, protocol option construction, and session restore
  tests cover the path, including global/task RPC overrides for all SFTP
  transport settings, rejection of a persisted zero connect timeout, and
  timeout of a pending core connection future. The protocol `connect_with_stream`
  path applies its configured deadline to both SSH handshake and authentication,
  and SFTP channel opening uses that timeout. The initial SFTP packet exchange
  uses the separately configured read timeout. The core deadline includes
  policy-based DNS/TCP and SSH negotiation/authentication. Core also observes
  pause, remove, and halt notifications while connecting, initializing the
  session, and opening the remote file; a focused regression test verifies a
  pending setup operation exits promptly after removal. A loopback server
  test verifies that a stalled channel open and stalled SFTP INIT response
  time out. A separate protocol loopback test confirms the server has entered
  password authentication before stalling, then verifies
  `connect_with_stream` returns `ConnectTimeout` within the configured
  connection deadline. The current focused protocol loopback run passes 12/12,
  and the CLI/RPC SFTP option tests pass 2/2. The core SFTP-filtered library
  run passes 34/34, including C API global/task propagation, shared option
  parsing, session restoration, and protocol-option construction. Its session
  persistence case requires a writable temporary directory; on the sandboxed
  Windows host, `TEMP` and `TMP` were pointed inside the workspace for this run.
  including early-EOF regressions for a truncated remote download and a local
  upload source that shrinks after its initial size was read; both paths return
  errors and close the remote handle. The suite also includes a loopback upload that
  reports a pre-existing remote prefix and verifies the
  resumed OPEN omits `TRUNC` and the first WRITE starts at the remote file
  size, an oversized-remote case that restarts with `TRUNC` at offset zero, and
  rejection of a resume offset beyond the local source. Upload CLOSE failure
  returns an error; a separate download case verifies that CLOSE failure after
  the full payload is received remains a cleanup warning and preserves the
  completed file. Cancellation coverage also verifies explicit remote CLOSE.
  Buffer bounds are checked at use time even when callers directly assign the
  public `TransferOptions::buffer_size` field. SFTP INIT advertises v3, and the
  implementation now accepts only the negotiated v3 wire format. Each packet
  read uses a single `sftp-read-timeout` deadline across all fragments; a
  slow-drip regression test verifies that periodic bytes do not reset it.
  Cancelling a public SFTP request while its send/response exchange is in
  flight invalidates the shared session, because a late response could
  otherwise be consumed as the next request's response. Cloned sessions share
  this invalidation state; later operations fail locally with
  `SessionPoisoned`. Core cancellation continues to discard the SSH connection
  instead of queueing CLOSE behind an outstanding read. A loopback regression
  cancels a stalled STAT and verifies the next request is never sent.
  `cargo check -p aria2-protocol --features sftp` and
  `cargo check -p aria2-core --features "bittorrent metalink sftp"` pass.
  Original aria2 has no matching SFTP-specific timeout options; these are Rust
  extensions.
- **Attribute updates:** existing `FileAttributes` keeps its zero-means-unknown
  SETSTAT mapping for source compatibility and omits grouped UID/GID or time
  fields if either member is zero. The additive `FileAttributePatch`
  and `set_stat_patch` API distinguishes omitted fields from explicit zero
  values; UID/GID and access/modification times are represented as pairs, as
  required by their SFTP v3 wire flags. Standalone uploads use the patch API
  when preserving permissions and timestamps.
  Protocol tests cover zero-valued field masks and SETSTAT encode/decode; a
  loopback SSH/SFTP fixture verifies the public `set_stat_patch` call sends
  explicit zero values in all four attribute groups. No remote server
  interoperability run has been performed.
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
- **Credential diagnostics:** SFTP command-creation logs omit the input URI,
  which can contain a password. `SshOptions` debug output redacts the username,
  password, and private-key passphrase; `SshConnection` debug output omits the
  username. This changes diagnostics only.
- **Verification:** CLI, RPC global/task, and C API global/task tests confirm
  `ssh-host-key-md` reaches each created download's `DownloadOptions`;
  protocol tests cover unknown-key rejection, first-use
  recording, known-key acceptance, and changed-key rejection. End-to-end
  loopback tests use the local mock SFTP server; focused tests verify host-key
  rejection is terminal even when retries are unlimited. The latest focused
  SFTP run passed 33/33 core tests and 23/23 standalone protocol transfer
  tests; protocol SSH option tests cover secret redaction. These include
  authentication, fingerprint checks, cancellation, resume,
  and transfer lifecycle. The full `aria2-protocol` all-features library suite
  passed 1333 tests with 2 ignored. No public remote SFTP
  server interoperability run has been performed. SSH trust behavior is not
  claimed compatible with original aria2 beyond explicit fingerprint checks.
- **Frame size policy:** SFTP frames are capped at 16 MiB including the
  four-byte length prefix. The decoder rejects a declared oversized frame as
  soon as the prefix is available, before waiting for or buffering its body;
  the encoder rejects frames above the same limit. This can reject unusually
  large but otherwise valid server packets and is an intentional compatibility
  deviation chosen to bound per-frame memory use.
- **Malformed packet handling:** length-prefixed fields are collected from
  bytes actually present before their contents are allocated, and a directory
  entry count is checked against the minimum possible encoded payload before
  reserving entries. Known packet fields must consume the entire payload, so
  unexplained trailing bytes are rejected. Encoding rejects field and packet
  lengths that do not fit the u32 wire fields, rather than silently truncating
  them; decoding checks the outer frame addition for platform-size overflow.
  **Verification:** packet tests reject an over-limit length prefix without a
  body and reject oversized encoded packets. Remote-server interoperability
  for frames near the cap has not been exercised.
- **Version negotiation:** the client sends `SSH_FXP_INIT` with version 3 and
  accepts the negotiated v3 version only, as required by the SFTP negotiation
  rule that the server selects the lower of its version and the client's.
  Versions above 3 are rejected because the current codec implements v3 only.
  Retry classification now preserves the protocol session's typed distinction:
  connection, channel I/O, and timeout failures are retryable; unsupported
  versions, malformed or mismatched packets, and unclassified SFTP status
  failures terminate the attempt without reconnecting. This matches the
  original command's abort path for SFTP operation errors and avoids retrying
  deterministic protocol/server failures. Focused core tests cover terminal
  INIT/version and generic server-status failures alongside retryable timeout
  and transport failures.
  The [IETF SFTP draft's protocol initialization section](https://datatracker.ietf.org/doc/html/draft-ietf-secsh-filexfer-13#section-4)
  is the primary reference for this negotiation behavior.

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
  Safety hardening rejects embedded CR/LF in FTP commands and unsafe
  server-provided local child paths; valid command sequences and ordinary
  single-component listing names are unchanged. FTP diagnostics omit `USER`
  and `PASS` arguments in standalone and core session paths.
- **Response bound:** standalone `FtpConnection` and injected
  `FtpControlSession` now share the original aria2 64 KiB maximum FTP response
  size. Bounded reads enforce the cap before appending an oversized line, so a
  peer cannot bypass the limit with one unterminated control line.
- **Response parsing:** standalone `FtpConnection` now adapts the same RFC 959
  response parser used by `FtpControlSession`, removing duplicate framing and
  multiline logic. Malformed or incomplete responses now return an error
  instead of a successful response with code `0`; the public `String` error
  type and legacy timeout, I/O, and size-limit messages remain.
- **ABOR response synchronization:** standalone `FtpConnection::abor` consumes
  the complete `426` then `226` abort sequence before allowing another command
  to use the control stream. A loopback test follows ABOR with NOOP and verifies
  the NOOP reads its own reply.
- **Transfer failure cleanup:** after RETR/LIST has entered its data phase,
  data-channel establishment, local file setup/write, exhausted transient-read
  retries, and cancellation close the data stream and attempt ABOR. Cleanup
  failure is logged while the original transfer error is preserved. A local
  passive data-connection timeout fixture verifies the ABOR exchange and a
  successful subsequent NOOP; a local file-setup failure fixture verifies the
  data socket closes and the control session handles a subsequent NOOP. Local
  file-write failure and exhausted data-read retry behavior are not separately
  fault-injected yet.
- **Transfer completion reply:** the standalone downloader accepts only a
  positive-completion 2xx reply after data EOF. A preliminary 1xx reply cannot
  be mistaken for successful completion; a loopback regression covers 150.
- **Known transfer size:** when FTP `SIZE` succeeds, file and in-memory
  downloads now require the transferred byte count plus any resume offset to
  equal the advertised size. An integration fixture returns one byte followed
  by 226 for a two-byte file and verifies the download fails while the control
  stream remains synchronized.
- **Standalone file resume:** before writing at a nonzero offset, the target
  must contain at least that many bytes; an incomplete local prefix is rejected
  instead of creating a hole. Any stale tail beyond the offset is truncated so
  old bytes cannot survive a shorter resumed transfer. Focused filesystem
  coverage verifies both cases; core's checkpoint-based resume remains managed
  by its separate engine path.
- **In-memory download allocation:** the standalone client caps the initial
  `Vec` reservation derived from remote `SIZE` at 1 MiB. This prevents a
  server-advertised size from forcing an immediate unbounded allocation; the
  buffer still grows as payload bytes arrive.
- **Transfer buffer allocation:** `FtpDownloadOptions::buffer_size` is public
  and may be directly mutated, so both file and in-memory transfer paths clamp
  it at use time to 1 byte through 1 MiB. This prevents zero-sized reads from
  being mistaken for EOF and avoids allocating an unbounded caller-supplied
  buffer; the default remains 64 KiB.
- **Command and local-path safety:** the shared FTP command writer rejects
  embedded CR/LF before writing any bytes. This blocks command injection from
  decoded FTP URI paths and applies to both the standalone connection and the
  core's injected control session. Recursive standalone downloads reject LIST
  entry names that are not one normal local path component, preventing
  absolute and parent-directory traversal from server-controlled names.
  Authentication logs omit username and password values. FTP command-creation
  diagnostics omit the raw URI and remote path, which could expose URI
  credentials; they retain the resolved host and port.
- **Current focused verification:** `cargo test -p aria2-protocol --lib ftp::`
  passes 81 tests, and
  `cargo test -p aria2-core --lib engine::ftp::download_command::tests` passes
  29 tests. A mock control stream verifies the injected
  session command sequence; protocol tests cover EPSV response fallback and
  propagation of a closed control stream; a core IPv4 loopback verifies that
  the command is PASV without an EPSV probe. An explicit FTPS mock verifies TLS
  on both control and data channels with certificate checking disabled in that
  local test.
  Windows test runs set `TEMP` and `TMP` to the writable
  `target/ftp-test-tmp` directory; the default OS temp directory does not allow
  the required control-file replacement in this sandbox. `cargo check -p
  aria2-protocol -p aria2-core`, workspace formatting, and `git diff --check`
  pass. `cargo check -p aria2-core --features "bittorrent metalink sftp"`
  passes after the current protocol changes. A public remote FTP/FTPS server was not used for interoperability
  testing. Focused tests cover bounded line reads and oversized responses in
  both public FTP client paths, CR/LF rejection at the shared command writer,
  rejection of unsafe recursive listing paths, and credential redaction in
  FTP command-log formatting. Earlier core FTP, FTP download end-to-end,
  proxy, and legacy integration checks are recorded in their respective audit
  entries.

- **FTPS TLS layering:** `aria2-protocol::ftp::tls::FtpsClient` builds a TLS
  connector once per download attempt and reuses it for the control and data
  channels. Core chooses the explicit/implicit and direct/proxy routes and
  keeps socket/address policy; TLS setup and RFC 4217 protection negotiation
  stay in the protocol crate. Existing FTPS wire order and certificate policy
  are unchanged. Focused local FTPS tests verify protected control and data
  channels; public remote-server interoperability remains unverified.

- **FTP control-session reuse:** `ftp-reuse-connection` now controls a
  process-scoped pool shared by core FTP download commands. The pool stores
  `FtpControlSession` with its buffered input, TLS client, connection context,
  and outbound policy. Its key isolates origin credentials, proxy credentials
  and endpoint, FTPS certificate/TLS mode, and outbound source addresses.
  Sessions return to the pool only after the transfer-completion response is
  read as `226`; failed, interrupted, or unsynchronized transfers close the
  session. The option description now correctly identifies the FTP control
  connection. A local two-download FTP fixture verifies a single TCP login
  serves both transfers and checks pool reuse statistics. Idle and maximum-age
  bounds are retained across leases, and engine shutdown clears idle sessions.
  Configuration reaches `DownloadOptions` from CLI, RPC global/task options,
  and C API global/task options; focused adapter tests pin false global values
  and task overrides. The RPC reserved-task update policy also carries the
  option into `RequestGroup` before execution.
  A second loopback fixture closes an idle pooled socket and verifies that the
  next task retries with a fresh authenticated session. This is intended to
  match original aria2's cross-download control-socket reuse while preserving
  the Rust protocol session's buffered state. Proxy, FTPS, and public-server
  interoperability cases remain unverified by these focused fixtures.

- **FTP I/O timeout propagation:** a new FTP control connection uses the
  request group's configured `timeout` while reading its greeting; each
  control-session lease then uses it for FTP commands, passive data connection
  or active accept, and the final transfer response. A pooled connection picks
  up the current task's timeout on every lease. When no task timeout is set,
  retain the previous 30-second greeting/command/data and 10-second completion
  fallbacks. This follows original aria2's request-group timeout use in
  `FtpNegotiationCommand.cc` and `FtpFinishDownloadCommand.cc`. Unit tests cover
  fallbacks, per-lease override, and stalled greeting/control responses.

## Metalink parsed execution data

- **Ownership:** Metalink XML parsing and file metadata remain in
  `aria2-protocol`. Initial expansion passes each normalized parsed file
  transiently through its in-memory request group to execution, so the command
  adapter does not parse the same XML again for every resource group. Restored
  groups reconstruct the parsed file from persisted source XML and file index.
  Commands retain parsed execution data; request groups retain source bytes for
  session reconstruction. `aria2-protocol::metalink::checksum_verifier` remains
  a standalone synchronous file-checksum API; engine execution instead uses
  core's shared asynchronous checksum service and Metalink piece verifier.
- **Policy:** Core's `engine::metalink` applies task-level file and resource
  selection, source ordering, retry, mirror failover, and staged-file
  publication. Protocol code supplies parsing, metadata, and ordering
  primitives; it does not decide the engine's retry or task lifecycle policy.
- **GID allocation:** the CLI requests each Metalink-generated group ID from
  the shared atomic `RequestGroupMan` allocator as groups are built. It no
  longer reparses documents to estimate a contiguous range after reserving
  only that range's first ID; this prevents concurrent task creation from
  receiving an ID already assigned to an expanded Metalink group.
- **Behavior:** mirror ordering, expected size, hashes, piece hashes, and
  BitTorrent metaurls use the same parser output as before. Parsing errors are
  still surfaced by command construction, before task execution.
- **Public model helper:** `MetalinkDocument::total_size` continues to sum
  known file sizes and preserve its empty-document result of `Some(0)`. It now
  returns `None` if known sizes overflow `u64`, instead of panicking in debug
  builds or wrapping in optimized builds. Core has no current caller; this
  affects only standalone protocol-library callers of the helper.
- **Transport and commit:** each mirror and torrent metadata URL uses the
  ordinary source-protocol adapters, with the task's DNS cache, proxy rules,
  outbound-address policy, and global limiter. HTTP, FTP, and feature-enabled
  SFTP URIs retain their original schemes through parsing and dispatch. Each
  source attempt writes to a mirror-specific sibling staging file; Metalink
  verifies expected size, whole-file hashes, and piece hashes before a durable
  atomic replacement publishes the destination. Metalink controls retries by
  limiting each child protocol command to one attempt and relays pause, halt,
  removal, and progress to that command. Pause and force-halt retain the
  mirror-specific staging file and its sibling control file so the same GID can
  resume that source. Explicit removal discards the unpublished staging file
  and control file; the incomplete payload is not published at the destination.
  HTTP source commands preserve the Metalink path's 30-second default connect
  timeout when the option is unset;
  an explicit task value is passed through unchanged. They also retain the
  former one-sequential-request-per-mirror pattern.
- **Verification:** focused Metalink constructor and execution tests cover
  direct mirrors, torrent-metaurl-only inputs, integrity data, grouped files,
  manager-restored source handling, parsed-file handoff, and restored-source
  fallback. An application regression verifies that expanding multiple
  resources advances the shared GID allocator past every generated group.
  The initial-path regression deliberately provides invalid source
  XML alongside a valid cached parsed file. A local FTP-over-HTTP-proxy fixture
  confirms FTP mirror dispatch and payload commit; a local HTTP proxy fixture
  confirms engine-injected proxy policy reaches Metalink payload traffic. The
  504 retry, pause, and torrent-metadata retry tests also pass through the new
  source-command path. Parser tests cover SFTP URI retention and FTP-family
  classification. Public-model regressions cover checked total-size overflow,
  known-size summation, and the empty-document result. No public remote-server
  or original-binary comparison was run; Metalink-over-SFTP wire
  interoperability and custom-DNS end-to-end
  behavior remain unverified. The lifecycle integration target passes 17/17,
  including pause/resume from the staging checkpoint, force-halt checkpoint
  retention, and removal cleanup.

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

## Product and protocol identity defaults

- **Ownership:** `aria2-protocol::identity` provides defaults for standalone
  protocol and core-library consumers. `aria2-core::ConfigManager` can receive
  embedding-product defaults; the `aria2` application supplies its binary
  identity when constructing `App`, while the C API session uses the core
  library defaults. RPC `aria2.getVersion` reports the binary package version.
  The resulting CLI and C API default HTTP/peer-agent strings can differ by
  product name and package version. This is intentional identity scoping; the
  `user-agent`, `peer-agent`, and `peer-id-prefix` options can override their
  respective defaults. The configured HTTP User-Agent is also applied to
  magnet exact-source HTTP metadata retrieval, alongside ordinary HTTP and
  WebSeed downloads. An explicit `User-Agent` value in the `header` list keeps
  precedence on those auxiliary download paths.
- **Verification:** protocol identity tests reject upstream `aria2/` strings;
  core option-registry tests check protocol defaults, and `ConfigManager`
  tests cover embedding identity injection with and without config-file
  loading. CLI tests check application defaults, and a C API session test
  checks its effective protocol identity defaults. CLI option parsing and C
  API global-option application are tested through `DownloadOptions`; the RPC
  backend test covers global defaults and per-call `aria2.addUri` override
  precedence. Loopback request tests verify
  the configured header on WebSeed and magnet exact-source traffic. A
  packet-level capture through each public adapter was not run in this audit.

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
- **SFTP:** with `async-dns=true`, the command adapter injects the engine's
  shared `DnsCache` for hostnames; IP literals bypass DNS. Lookup and address
  attempts run inside the configured SFTP connect deadline and TCP sockets
  still pass through `OutboundNetworkPolicy`. With `async-dns=false`, that
  policy uses Tokio's system resolver. Local tests confirm both shared-cache
  lookup and explicit-DNS resolution through to the selected TCP peer. The
  explicit-server fixture stops at TCP accept; SSH negotiation/authentication
  against a remote SFTP server remains unverified.

## C API process-engine option setup

- **Startup mapping:** C API session construction applies the process-owned
  outbound interface policy (`interface` before `multiple-interface`), DNS
  timeout and explicit DNS servers, server-stat freshness and persistence,
  scheduler and global rate limits, plus BitTorrent tracker, uTP, handshake,
  and LPD settings when that feature is enabled. The session starts from core
  defaults without consulting process environment variables or an implicit
  config file; only explicitly supplied session options affect its startup.
  CLI and C API use the same `DownloadEngine::from_process_options` core
  boundary after their entry-point-specific option loading.
- **Verification:** a focused unit test inspects the constructed engine's
  outbound policy, DNS cache, and server-stat configuration with custom values;
  it passes both with and without the BitTorrent feature. A configuration
  constructor test verifies the no-config defaults; ambient environment and
  implicit-file isolation are enforced by the constructor selected in C API
  session setup.
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

## HTTP CONNECT authentication response timeout

- **Behavior:** parsing the CONNECT response head and draining a 407
  authentication body use the caller's configured `read_timeout`. This avoids
  an internal fixed two-second deadline that could contradict the timeout
  already applied to the response head. Body discard limits remain bounded.
  The protocol rejects conflicting or malformed `Content-Length` values and
  accepts chunked framing only when `chunked` occurs once as the final transfer
  coding. It rejects an unframed 407 and stops the auth retry because the body
  boundary cannot be determined safely while reusing the proxy stream. This
  intentionally rejects nonconforming or close-delimited 407 responses that
  cannot support an in-place retry.
- **Scope:** HTTP CONNECT callers in core pass the same configured timeout to
  the protocol session. This does not change proxy selection, auth retry count,
  authentication scheme policy, or the ordinary tunnel success path.
- **Verification:** protocol tests cover configured timeout enforcement,
  preservation of bytes following Content-Length and chunked 407 bodies, and
  rejection of unframed bodies, conflicting lengths, and non-final or repeated
  `chunked` coding.

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
- **Input extensions:** `changeGlobalOption` accepts native JSON number and
  boolean scalars as an additive Rust convenience. Cumulative arrays retain
  only string members; arrays for non-cumulative options are ignored. Empty
  arrays are no-ops except for `bt-tracker-source`, where the explicitly
  user-selected behavior is to clear the list. Original aria2 treats an empty
  array as a no-op. Registry and core-backed tests cover these cases. Task
  `changeOption` follows the original string-only scalar and
  string-members-only array behavior.
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
  Task `changeOption` accepts string scalars and string array members, skipping
  non-string scalar values and non-string array members as the original RPC
  contract does. Its dedicated input normalizer is separate from response
  option normalization, which converts numbers and booleans to strings.
  Normalized input then delegates validation and immediate/pending lifecycle
  semantics to core; focused tests cover these conversions, successful updates,
  ignored unknown keys, and atomic rejection of an invalid known value. An
  XML-RPC `/rpc` end-to-end test decodes a `changeOption` struct, applies it,
  and reads the task option back. A core-backed `changeGlobalOption` regression
  verifies that adapter validation rejects the whole batch before changing a
  valid sibling option or emitting runtime commands.
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
