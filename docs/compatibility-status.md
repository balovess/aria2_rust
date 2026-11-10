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
