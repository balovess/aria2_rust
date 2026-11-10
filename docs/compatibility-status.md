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
  entry points. They configure subsequently created download tasks.
- **Verification:** an engine-level pause/resume test lets a resumed range
  complete, then rate-limits subsequent ranges and verifies the command exits
  with an error instead of remaining active.

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

- **Extension:** `sftp-connect-timeout` and `sftp-read-timeout` configure the
  SSH connection and SFTP I/O read timeouts independently, in seconds. Defaults
  remain 15 and 30 seconds; each accepts 1 through 600 seconds.
  `sftp-compression` enables SSH compression (default `false`),
  `sftp-keepalive-interval` defaults to 60 seconds and accepts 0 through 600
  (`0` disables keepalives), and `sftp-preferred-ciphers` sets the ordered
  comma-separated cipher preference (empty means library defaults). These
  settings are per download and do not change the meanings of aria2's generic
  HTTP/FTP `connect-timeout` and `timeout` options.
- **Propagation:** CLI/configuration values, RPC `addUri` options, and session
  persistence feed the SFTP protocol connection builder. `changeOption` stores
  changes for reserved downloads and defers them on active downloads until the
  task is restarted.
- **Verification:** focused CLI, RPC option validation, and protocol option
  construction tests cover the propagation path. Original aria2 has no matching
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
  SFTP interoperability is validated separately; SSH trust behavior is not
  claimed compatible with original aria2 beyond explicit fingerprint checks.

## FTP control-session layering

- **Internal design:** `aria2-protocol::ftp::connection::FtpControlSession`
  wraps a caller-established stream and owns FTP command sequences, command
  framing, RFC 959 response parsing, and path, SIZE, MDTM, PASV/EPSV, and
  EPRT/PORT handling. Core retains address selection and outbound policy, proxy
  and FTPS setup, data-channel connection/listener policy, task lifecycle, and
  file state.
- **Compatibility intent:** this is an internal relocation. FTP command order,
  response acceptance, passive-address policy, retries, and output behavior are
  intended to remain unchanged.
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
  testing.

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
