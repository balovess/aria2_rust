# Compatibility Status

Last verified: 2026-08-12
Reference implementation: aria2_original/  
Rust workspace version: 0.2.8
Public product identity: aria2-rust 0.2.8

This is the current status source for the migration. The file-level records
under docs/migration/ and the historical plans under .trae/ are useful
evidence, but their checklists do not establish behavioural compatibility.

## Status Rules

| Status | Meaning |
| --- | --- |
| FULL | Rust covers the relevant original behaviour and has focused protocol or end-to-end evidence. |
| PARTIAL | The main path exists, but a documented behaviour, lifecycle, platform, or interoperability gap remains. |
| UNVERIFIED | Code exists, but the required protocol/E2E evidence has not been run or is not reproducible on the current host. |
| MISSING | No Rust equivalent exists for an in-scope original capability. |
| N/A | The original implementation is replaced by an intentional Rust/platform mechanism and the replacement is documented. |

“Source file compared” is not the same claim as FULL. The 530 C++ source
units catalogued in docs/MIGRATION.md measure audit coverage only.

## Compatibility Policy

The public behavior of `aria2_original` is a hard compatibility contract. This
includes CLI and configuration names/defaults, JSON-RPC/XML-RPC/WebSocket wire
shapes, authentication, error codes and HTTP status codes, session files,
notifications, task states, and the behavior relied on by existing clients
such as browser extensions. A Rust implementation is not compatible merely
because it exposes a similarly named method.

Product identity is an intentional documented exception: `aria2.getVersion`,
the CLI version action, default User-Agent values, and BitTorrent peer identity
identify `aria2-rust` at the current workspace version. Their surrounding wire
formats and protocol behaviour remain compatible.

Internal modules may be redesigned around Rust ownership, typed errors,
async I/O, and lower allocation or lock overhead, but those changes must stay
behind the public seam. A Rust-only feature is an extension: it must be
additive, explicitly documented, and must not change the result observed by an
original client using an original request. Features absent from
`aria2_original` (for example FTPS) are measured as extensions and are not
substitutes for missing original behavior.

## Current Module Matrix

| Area | Rust implementation | Status | Main evidence or remaining gap |
| --- | --- | --- | --- |
| Engine and scheduling | aria2-core/src/engine/ | PARTIAL | Typed command loop, generation-based completion accounting, `CancellationToken` shutdown, pause/unpause requeueing, runtime concurrency and global rate updates are covered. Shared retry policy now has source-backed `max-tries` semantics across sequential HTTP, concurrent segments, and FTP; full parity across allocation and all protocol commands is not yet proven. |
| HTTP/HTTPS | aria2-core/src/http/, aria2-protocol/src/http/ | PARTIAL | Focused parser and download coverage exists, including existing-file naming, control-file cleanup, preallocation-safe resume recovery, multi-URI resume failover, HTTP 200 responses that ignore a requested Range (`CannotResume` by default or fresh restart according to `always-resume`/`max-resume-failure-tries`), and an E2E check that `max-tries` counts total GET attempts with `0` meaning unlimited. Core owns production orchestration; `aria2-protocol::http::client` remains a separate compatibility-layer client with no core production callers, and broader original-binary interoperability remains unverified. |
| FTP/FTPS | aria2-core/src/ftp/, aria2-protocol/src/ftp/ | PARTIAL | Original FTP active/passive/auth behavior has focused coverage, including a canonical multiline response parser with the C++ 64 KiB receive limit, the original PASV control-peer target rule, and an E2E check that `max-tries` counts total control attempts. Live-server and client interoperability evidence is incomplete. FTPS is a Rust-only additive extension: explicit/implicit control and data TLS paths exist, the plaintext downgrade regression is covered, and positive TLS-server interoperability is still unverified. |
| SFTP | aria2-protocol/src/sftp/, aria2-core/src/engine/sftp_download_command/ | PARTIAL | A local `russh` SFTP server E2E now verifies password acceptance and rejection, aria2_original's `sha-1=<hex>` host-key pin acceptance and mismatch rejection, missing-file mapping, complete output, and resume from an existing local prefix (`test_e2e_sftp_download`: 6 passed). Focused protocol and core command suites also pass (137 and 16 tests). Interoperability with third-party SFTP servers, public-key authentication, and the complete original error/extension matrix remain unverified. Known-hosts persistence is not part of aria2_original's `ssh-host-key-md` contract. |
| BitTorrent | aria2-protocol/src/bittorrent/, aria2-core/src/engine/bt_* | PARTIAL | Core protocol pieces exist. `index-out` now applies the original 1-based `INDEX=PATH` mapping to both `DownloadContext` and the actual single/multi-file writers; TCP listen-port ranges try ports in order and have occupied-port regression coverage. `bt-prioritize-piece` now uses the original typed `head[=SIZE],tail[=SIZE]` parser and a file-boundary priority wrapper over rarest-first, with focused parser/picker/index tests. Incoming listener ownership, dependency graph, and full scheduler/seeding parity remain. |
| DHT and trackers | aria2-protocol/src/bittorrent/dht/ | PARTIAL | Production paths and tests now use the protocol crate as the single canonical DHT implementation; the former `aria2-core/src/dht/` source tree is no longer exported. DHT port ranges now try the ordered list and fall back after an occupied first port; complete live-network evidence is still missing. |
| Metalink | aria2-protocol/src/metalink/, aria2-core/src/engine/metalink_* | PARTIAL | V3/V4 parsing, filtering, resource downloads, manager-owned GID allocation, relative-URI base propagation, and metadata/payload graph terminal states have focused regression coverage. Same-metaurl multi-file grouping, full `follow-torrent=mem` semantics, session graph restoration, and live protocol interoperability remain open. |
| Integrity and resume | aria2-core/src/checksum/, aria2-core/src/session/ | PARTIAL | Sequential resume detection, defunct-control-file cleanup, existing-file policy, preallocation-safe offset writes, `always-resume`, and `max-resume-failure-tries` multi-URI behavior have focused unit/HTTP E2E evidence. Session serialization preserves original option names and non-default values for resume policy, trackers, port ranges, piece sizing, FTP/auth/netrc settings, plus the original 16-hex-digit GID form; Rust-only fields remain extensions. The result-code seam now contains exactly the original wire values `0..32`; `paused` remains a separate task status and cannot leak a Rust-only error code. Concurrent control-file lifecycle, piece-level resume semantics, and integrity entry callbacks remain incomplete or unverified. |
| RPC and WebSocket | aria2-rpc/src/ | PARTIAL | JSON-RPC/XML-RPC/WebSocket surfaces, token/Basic authentication, aria2-compatible error/status mapping, feature-specific method/notification discovery, feature-aware `getVersion`, browser-facing CORS preflight headers, and real HTTP E2E coverage exist. CORS is disabled by default as in `aria2_original`; explicit `rpc-allow-origin-all=true` enables wildcard headers, and `Access-Control-Max-Age` matches the original at `1728000`. XML-RPC execution faults use HTTP 200 + `faultCode=1`, while parser/value failures match the original HTTP 400 empty-body contract. `getServers` is active-only and reports only real in-flight requests; waiting, paused, stopped, or unknown GIDs return execution error code 1. `getSessionInfo` now generates one 20-byte random session key per engine and exposes the original 40-character lowercase hexadecimal representation. The catalog is 33 core methods plus 2 BitTorrent and 1 Metalink method when enabled; notifications are 5 core plus 1 BitTorrent event when enabled. `aria2.forceUnpause` is rejected as an unknown original method and omitted from `system.listMethods`, keeping original-client discovery exact. Task creation and runtime changes share core validation; `RequestGroup` owns a source-derived `setInitialOption(true)` request snapshot and transfers its effective state to `DownloadResult` when a task stops, excluding process-only RPC settings and Rust-only session metadata. `getOption` therefore keeps the original task state for both live and stopped GIDs, including only changes already applied to the task; later `changeGlobalOption` calls affect future tasks without rewriting existing ones. `getGlobalOption` uses registry-owned original wire metadata: defined hidden or deprecated original values remain observable, no-default values stay absent until configured, `rpc-secret` is withheld, and Rust-only uTP fields cannot leak into an original-client response. `changeUri` now honors the optional zero-based insertion position after deletions, matching the original ordering and count result; task-creation positions share the same rejection rules for negative values. `tellStatus`, `tellActive`, `tellWaiting`, and `tellStopped` honor the original optional `keys` field filter while preserving full output when omitted, and waiting/stopped pagination supports the original negative-offset semantics. Full original-client interoperability, including the browser-extension matrix and complete XML-RPC client coverage, remains unverified. |
| CLI and options | aria2/src/app/, aria2-core/src/config/ | PARTIAL | `OptionDef::parse_value` remains the shared typed seam for CLI/config/RPC validation, and `App::load_cli_args` now propagates validation failures instead of silently discarding them. Regression coverage proves invalid `--split=0` and unknown `--file-allocation` values are rejected before engine startup; startup coverage proves `--no-conf` skips an explicit config file as in the original. `IntegerRange` preserves ordered range wire values, `IndexOut` uses one cumulative `INDEX=PATH` parser for validation and BT execution, and `bt-prioritize-piece` validates the original `head[=SIZE],tail[=SIZE]` grammar through the same registry seam. The original short-option contract is covered for registry mappings, including `-a`/`-p`/`-P`/`-R`/`-u`/`-Z`/`-S`/`-T`/`-M`, and `-h`/`-v`/`-V` actions. `-h`/`--help[=TAG|KEYWORD]` now preserves the optional-argument/getopt boundary, renders before engine startup, and filters by long-option keyword or supported help groups. A source-derived audit now finds all 198 original public option names represented in Rust CLI help; runtime behavior for newly exposed process options, exact defaults/changeability, exact help-tag membership/text, and full E2E proof remain open. CLI product identity and version output intentionally belong to `aria2-rust`. Rust-only names are retained only where they are documented extensions or compatibility aliases and still require ownership review. |
| Public C API/ABI | aria2_original/src/aria2api.cc, src/includes/aria2/aria2.h | PARTIAL | `aria2-core/src/c_api.rs` and `bindings/c/` provide a tested opaque-handle `extern "C"`/cdylib migration interface. It is intentionally source-level and is not binary-compatible with the original C++ classes or STL ABI. |

## Verification Evidence

### Current Compatibility Slice (2026-08-10)

The project emits one product identity. The Cargo package version
(`aria2-rust` 0.2.8 on this checkout), CLI version action and startup banner,
RPC `getVersion`,
default HTTP/tracker User-Agent, BitTorrent peer agent, and BitTorrent
extension handshake use the same release source.

- `user-agent` and `peer-agent` registry defaults use `aria2-rust/0.2.8`.
  The peer-ID prefix is `A2-RUST-`; the default peer ID is generated once per
  process and remains exactly 20 bytes.
- RPC message shapes, method names, authentication, errors, notifications,
  routes, and protocol semantics remain the compatibility surface. The product
  identity values above are intentional, documented differences.
- `aria2.forceUnpause` is rejected as an unknown original RPC method and is
  not advertised. The Rust-only root HTTP information endpoint and `/ws`
  WebSocket alias were removed; `/` and `/ws` return 404, while `/jsonrpc`
  remains the compatible JSONP/JSON-RPC and WebSocket route.

This slice was checked with `cargo fmt --all -- --check`, the focused
`identity::tests::product_identity_uses_the_package_version`,
`bittorrent::peer::id::tests::test_generate_peer_id`, and
`handlers::handler_tests::test_get_version_uses_product_version` regressions,
the complete `test_cli_options` target (105 passed),
`cargo build -p aria2 --all-features -j 1` with an observed
`aria2c --version` value of `aria2c 0.2.8`, and
`cargo clippy -p aria2 --all-targets --all-features -- -D warnings`.
The RPC catalog additionally passed the independent
`handlers::system::tests::test_list_methods` regression and the real HTTP
`e2e_system_list_methods_returns_array` and
`e2e_force_unpause_is_rejected_as_an_unknown_original_method` tests.
The complete `test_e2e_all_rpc_methods` target passed 55 tests.
The complete `test_e2e_http_server` target passed 46 tests, covering JSON-RPC,
JSONP, XML-RPC, WebSocket, authentication, CORS, endpoint routing, and
compatible HTTP error contracts.
The `aria2` all-targets suite also passed, with 3 pre-existing daemon tests
ignored by design.
It does not establish complete original-client or end-to-end download
compatibility.

Verified on 2026-08-09 with single-job builds where needed:

~~~text
cargo fmt --all -- --check                              PASS
cargo check -p aria2-core --all-features --tests -j 1    PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings PASS
cargo clippy -p aria2-protocol --all-targets --all-features -- -D warnings PASS
cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings PASS
cargo test -p aria2-core --test test_e2e_download --all-features -- --test-threads=1 26 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib --all-features -- --test-threads=1 3307 passed, 1 ignored, 0 failed
cargo clippy --workspace --all-targets --all-features -- -D warnings PASS (prior checkpoint; not rerun in this incremental checkpoint)
cargo test -p aria2-core --all-features c_api --lib   PASS
cargo test -p aria2-protocol --all-features -j 1        872 passed, 4 ignored
aria2-rpc all-feature test targets (listed in Latest RPC Wire Checkpoint) 402 passed, 0 failed
cargo test -p aria2 --lib application_rpc_does_not_enable_cors_by_default --all-features -- --test-threads=1 1 passed, 0 failed
cargo test -p aria2 --all-features --tests -j 1 -- --test-threads=1 292 passed, 3 ignored, 0 failed
cargo build -p aria2 --all-features -j 1                PASS
npm run typecheck                                        PASS
npm run build                                            PASS
ARIA2_RUST_BIN=target/debug/aria2c.exe npm test          123 passed
PYTHONPATH=.codex-python-deps python -m pytest -p no:cacheprovider 137 passed
~~~

Latest FTP/FTPS checkpoint (2026-08-09):

~~~text
cargo check -p aria2-core --all-features --tests -j 1                         PASS
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings        PASS
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib ftp --all-features -- --test-threads=1
  200 passed, 0 failed, 1 ignored
~~~

The FTPS negative regression proves that an `ftps://` request does not accept
a plaintext FTP server. A positive explicit/implicit FTPS server exchange is
still unverified because the current fixture set has no valid reusable server
certificate/key pair. FTPS remains an additive Rust extension and is not an
original-aria2 compatibility requirement.

The production `FtpDownloadCommand` now follows the original PASV connection
target rule: it validates and logs the advertised host, but opens the data
socket to the control connection's peer address. The regression fixture
advertises `127.0.0.2` while listening on `127.0.0.1`; the download succeeds,
matching `aria2_original/src/FtpNegotiationCommand.cc` and avoiding failures
with NATed or misconfigured FTP servers. This is local protocol evidence, not
complete live-server or original-client interoperability evidence.

Latest retry-contract checkpoint (2026-08-09):

~~~text
cargo clippy -p aria2-core --all-targets --all-features -- -D warnings       PASS
cargo test -p aria2-core --test test_retry --all-features -- --test-threads=1
  15 passed, 0 failed
cargo test -p aria2-core --test test_e2e_retry_contract --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --test test_e2e_ftp_download --all-features -- --test-threads=1
  25 passed, 0 failed, 2 ignored
cargo test -p aria2-core --lib ftp --all-features -- --test-threads=1
  200 passed, 0 failed, 1 ignored
~~~

The retry policy matches `aria2_original`: default `max-tries=5`, the value is
the total number of attempts, and `max-tries=0` is unlimited. This checkpoint
is limited to retry behavior and does not change the overall HTTP/FTP or
workspace status, nor does it establish original-client interoperability.

Latest browser-facing RPC HTTP checkpoint (2026-08-09):

~~~text
cargo test -p aria2-rpc --test test_e2e_http_server --all-features -- --test-threads=1
  43 passed, 0 failed
cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings
  PASS
~~~

The OPTIONS preflight response now carries the same `Access-Control-Max-Age:
1728000` value as `aria2_original/src/HttpServerBodyCommand.cc`. This verifies
the browser-facing header at the HTTP seam. The application startup seam also
verifies that the default registry leaves `rpc-cors-domain` unset, so no
`Access-Control-Allow-Origin` header is emitted unless CORS is explicitly
configured. This does not establish the complete original browser-extension or
client interoperability matrix.

Focused HTTP resume regression evidence (2026-08-09):

~~~text
cargo test -p aria2-core --test test_e2e_download resume_failure --all-features -- --test-threads=1  4 passed, 0 failed
cargo test -p aria2-core --lib request::request_group::options::tests::rpc_option_map_uses_aria2_wire_strings --all-features -- --exact  PASS
~~~

Latest option/BitTorrent/port-range checkpoint (2026-08-09):

~~~text
cargo test -p aria2-core --lib config::option::tests --all-features -- --test-threads=1 34 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_download_command_tests --all-features -- --test-threads=1 34 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_peer_listener::tests --all-features -- --test-threads=1 4 passed, 0 failed
cargo test -p aria2-protocol --lib bittorrent::dht::engine::tests --all-features -- --test-threads=1 6 passed, 0 failed
cargo fmt --all -- --check PASS
~~~

This checkpoint verifies the shared `OptionDef`/RPC validation seam, cumulative
`index-out` parsing and actual BitTorrent output-path application for single and
multi-file torrents. It also verifies ordered TCP and UDP port-range fallback
when the first candidate is occupied. These are focused compatibility results;
they do not establish browser-extension interoperability or workspace-wide
end-to-end completion.

The current runtime policy seam additionally verifies that every name in the
global-changeable, initial-request, immediate-changeable, and reserved-change
sets resolves to the canonical `OptionRegistry` without duplicates:
`runtime_policy_names_are_registered_without_duplicates` passed, and
`cargo clippy -p aria2-core --all-targets --all-features -- -D warnings`
passed.

Latest BitTorrent piece-priority checkpoint (2026-08-12):

~~~text
cargo check -p aria2-core --all-features -j 1                         PASS
cargo test -p aria2-protocol --lib bittorrent::piece::picker --all-features -- --test-threads=1
  37 passed, 0 failed
cargo test -p aria2-core --lib config::option --all-features -- --test-threads=1
  39 passed, 0 failed
cargo test -p aria2-core --lib engine::bt_piece_selector --all-features -- --test-threads=1
  13 passed, 0 failed
~~~

Compared with `aria2_original/src/OptionHandlerImpl.cc`,
`aria2_original/src/util.cc`, and `aria2_original/src/RequestGroup.cc`,
`bt-prioritize-piece` now has no synthetic `rarest` default, accepts only the
original `head[=SIZE],tail[=SIZE]` grammar with `K`/`M` units, computes the
file-boundary piece set from global file offsets, and tries that set before
the normal rarest-first picker. This closes the focused option/scheduler
semantic gap; full BitTorrent scheduler, seeding, and live interoperability
evidence remains open.

Latest CLI optional-argument checkpoint (2026-08-09):

~~~text
cargo test -p aria2 --test test_cli_options --all-features regression_h_does_not_set_listen_port -- --exact PASS
cargo test -p aria2 --test test_cli_options --all-features regression_help_selector -- --exact PASS
cargo test -p aria2 --test test_cli_options --all-features regression_help_rendering_filters_options -- --exact PASS
cargo check -p aria2 --all-features --tests -j 1 PASS
~~~

The CLI now accepts `-h`/`--help` with an optional value only in the
equals-attached form, preserving a following positional URI; the short
`-htimeout` form and the original `-h=timeout` truncation rule are covered as
well. `#tag` and option-name selectors are rendered before configuration or
engine startup.
The renderer still needs a generated comparison against the original option
inventory for exact tag membership, wording, and supported option semantics;
this checkpoint is parser and lifecycle evidence, not full CLI parity. Product
branding and version text are intentionally owned by `aria2-rust`.

The original public option inventory comparison on this checkout reports 198
public names in `aria2_original` and all 198 represented by the Rust CLI help;
the Rust surface currently has additional documented or compatibility-only
names. This proves name coverage only. It does not prove that every option's
runtime effect, default, changeability, hidden/deprecated status, or help text
matches the original.

Latest process-level RPC checkpoint (2026-08-09):

~~~text
cargo build -p aria2 --all-features -j 1                                      PASS
cargo test -p aria2 --lib app::rpc::bridge_tests --all-features -- --test-threads=1 7 passed, 0 failed
cargo test -p aria2 --test e2e_arianng_rpc_client -- --test-threads=1        1 passed, 0 failed
cargo clippy -p aria2 --test e2e_arianng_rpc_client -- -D warnings           PASS
cargo test -p aria2 --test e2e_arianng_rpc_client --all-features -- --test-threads=1 1 passed, 0 failed
cargo clippy -p aria2 --test e2e_arianng_rpc_client --all-features -- -D warnings PASS
~~~

The built `aria2c.exe` was started with `--enable-rpc=true` and no initial URI.
An original JSON-RPC envelope reached the live process: `aria2.getVersion`
returned the then-current package version, `system.listMethods` returned 36
methods including `aria2.addUri`, and `aria2.shutdown` returned
`OK. 0 active downloads paused.`
The process exited within five seconds. A separately occupied RPC port now
fails startup before the process-wide download-event bridge is registered.
The process-level AriaNg regression now starts a separate `aria2c` with
`rpc-secret`, sends the real `system.multicall` refresh shape with a `token:`
in every sub-call (and no envelope token), verifies the original nested result
wrappers and stringified global statistics, then shuts the process down through
`aria2.shutdown`. This is default and all-features evidence for the actual
CLI/listener/HTTP/auth/dispatch lifecycle. It does not yet prove the complete
Chrome/browser-extension matrix or all original-client workflows.

Latest RPC compatibility checkpoint (2026-08-09): unknown and
non-changeable options are ignored as in `aria2_original`; a recognized option
with an invalid value returns execution error `code=1` and HTTP 400 for
JSON-RPC. `addUri`, `addTorrent`, and `addMetalink` use the same core parser as
runtime option updates. Only registry-declared cumulative options accept RPC
arrays; ordinary options cannot be silently converted from arrays. The
checkpoint includes 224 library tests, 18 integration tests, 55 all-method
HTTP tests, 46 HTTP/WebSocket/XML-RPC route tests, 5 server-config tests, 4
HTTPS tests, 31 mock-server tests, 9 header/progress tests, and 10 stress
tests, for 402 passed and 0 failed. The aggregate command completed in the
current checkpoint. The
active/reserved task changeability policy is centralized with the global
policy in `aria2-core/src/config/runtime.rs`; `request_group` only preserves
the historical re-export path. This is a RPC-scope result only; it does not
establish workspace-wide completion or full compatibility with every original
browser client.

For live groups and stopped results, `aria2.getOption` now follows
`GetOptionRpcMethod::process()` directly: `RequestGroup` owns the creation
snapshot, core projects it through the original
`OptionHandlerFactory.cc::setInitialOption(true)` policy, and transfers the
effective state to `DownloadResult` during the terminal lifecycle transition.
This shares one rule across CLI-created tasks, RPC-created tasks, and
session-restored groups; it prevents listener/authentication settings and
`aria2-rust-*` session metadata from leaking through the task wire response.
Only task changes that have actually taken effect are overlaid, so a later
`changeGlobalOption` changes future groups but cannot rewrite an existing
group or stopped result. The header/progress and process-level
config-inheritance targets cover active-task behavior, and the stopped-task
process regression covers a CLI task after `forceRemove`, including an applied
`changeOption` override.

The RPC regression target also covers `aria2.changeUri`'s optional `pos`
parameter against `aria2_original/src/RpcMethodImpl.cc`: deletions happen
first, added URIs are inserted in input order at the requested zero-based
position, and the response remains `[delCount, addCount]`. The focused
position and legacy append tests both pass. This is method-level evidence and
does not change the overall RPC status from `PARTIAL`.

The status-query regression also covers the original optional `keys` argument:
an omitted or empty list keeps the complete status object, while a non-empty
list returns only requested known fields. The same shared filter is used by
`tellStatus`, `tellActive`, `tellWaiting`, and `tellStopped`; the focused
`tellStatus`/`tellWaiting` test passes, including unknown-key omission.
Pagination now requires the original offset/count arguments, accepts negative
offsets for reverse-from-end queries, and rejects negative counts with an
aria2 execution error. Task-creation position parsing uses the same optional
argument seam and rejects negative positions before registering a GID.

The JSON-RPC wire adapter now has direct source-backed regression coverage for
the original envelope rules: `jsonrpc` is ignored, omitted `params` becomes an
empty positional list, missing `id` and object params become object-level
errors, non-object batch elements are skipped, and an empty batch returns
`[]`. This improves wire compatibility but does not prove interoperability
with every browser extension.

The legacy `/jsonrpc` GET/JSONP adapter is also source-backed against
`aria2_original/src/json.cc`: it matches raw `key=value` prefixes, percent-
decodes only `params`, preserves the original permissive Base64 failure path,
keeps `method`/`id`/`jsoncallback` text unnormalized, and returns the original
parse error for a request without a query. JSONP callbacks are emitted using
the original verbatim callback rule for strict wire compatibility; callers
must therefore use the same authentication and deployment protections as the
original endpoint. Explicitly empty `params=` is also omitted from the
generated request object, matching the original string builder. Basic Auth
follows the original empty-password rule: an empty `rpc-passwd` is treated as
username-only authentication. The new GET/JSONP and Basic Auth regressions
pass as part of the 46-route HTTP/WebSocket/XML-RPC target.

Single-response JSON-RPC errors also send `Connection: close`, matching
`aria2_original/src/HttpServerBodyCommand.cc` after `disableKeepAlive()`;
successful requests and batch responses retain the normal HTTP connection
reuse path. This is covered by
`e2e_jsonrpc_errors_close_the_http_connection_like_original`.

The XML-RPC adapter now also has source-backed HTTP contract coverage against
`aria2_original/src/HttpServerBodyCommand.cc`: parser or XML value conversion
failures return HTTP 400 with an empty body and no `Content-Type`, while a
successfully parsed method execution failure returns HTTP 200 with an XML
fault whose `faultCode` is `1`. The current all-features RPC target set is 402
passed and 0 failed after this and later wire regressions were added.

The XML-RPC value adapter was compared with
`aria2_original/src/XmlRpcRequestParserStateImpl.cc` and
`test/RpcHelperTest.cc`. Explicit XML strings now preserve leading and trailing
whitespace, and `<double>` request values are forwarded as strings, matching
the original parser's observable string-state behavior for `<double>`.
The focused `xml_rpc` target reports 15 passed and 0 failed; the full HTTP
target remains 46 passed and 0 failed. This is a parameter-coercion seam
regression, not complete XML-RPC client interoperability evidence.

The `getSessionInfo` value was compared with
`aria2_original/src/DownloadEngine.cc` and
`aria2_original/src/RpcMethodImpl.cc`. Rust generates the random key once when
`RpcEngine` is constructed and emits the same 40-character lowercase hex value
through JSON-RPC, XML-RPC, and WebSocket dispatch. The handler regression
asserts the exact length and alphabet as well as per-engine stability; the
legacy `rpc_helpers::generate_session_id` path now forwards to the canonical
`SessionInfo` implementation.

The session option adapter was compared with
`aria2_original/src/SessionSerializer.cc` and `OptionParser.cc`. It keeps
aria2's `load-cookies` key while accepting Rust's `cookie-file` input alias,
preserves ordered `listen-port`/`dht-listen-port` ranges, and restores the
non-default option set through the typed `DownloadOptions` parser. The focused
session-entry target reports 19 passed tests; this is session compatibility
evidence, not proof that every session lifecycle or original client workflow
is complete.

The `aria2.getServers` adapter now follows the original active-only contract:
it emits one file-index entry per active file and includes only requests that
currently have peer statistics. Configured mirrors are not emitted as fake
servers; waiting, paused, stopped, and unknown GIDs map to execution error
code 1 with HTTP 400 for JSON-RPC.

The core command `cargo test -p aria2-core --lib --all-features --
--test-threads=1` completed with exit code 0. Its library target reported
3,293 passed and 1 ignored. Integration and performance targets are covered by
the separate commands listed above; the aggregate workspace command
`cargo test --workspace --all-features -j 1` has not been used as a green gate,
so this document does not claim one workspace aggregate run.

The current remaining acceptance gaps are:

- The original C++ class/STL ABI is not and cannot be claimed as binary
  compatible; the Rust project currently provides a separate opaque-handle
  source-level C migration ABI. Header/API coverage beyond the current
  session, control, and snapshot surface is still incomplete.
- Metalink dependency lifecycle now has explicit metadata-success,
  direct-mirror-fallback, and terminal-failure states. Same-metaurl
  multi-file grouping, `follow-torrent=mem`, session graph restoration, and
  real HTTP/FTP/SFTP/DHT/BitTorrent Metalink interoperability still need
  implementation or reproducible evidence.
- HTTP, FTP, and DHT still have multiple layers with incomplete canonical
  ownership; third-party SFTP plus live FTP/DHT interoperability remains
  unverified here. The local SFTP fixture is reproducible evidence for the
  password, SHA-1 pinning, missing-file, full-download, and resume paths only.
- HTTP sequential resume now distinguishes a Range request answered with 200:
  default `always-resume=true` returns `CannotResume`, while
  `always-resume=false` restarts from byte zero. Multi-URI failover and
  `max-resume-failure-tries` are covered for the sequential HTTP path; the
  concurrent and cross-protocol retry matrices remain open.
- CLI/config defaults, changeability, error semantics, exact help-tag and
  help/version text parity, and the complete original-client matrix still need
  generated comparison and E2E proof. The optional-argument parser boundary is
  covered, but that does not establish complete CLI output parity.
- Some network-oriented tests remain intentionally ignored; ignored tests are
  not counted as compatibility evidence.
- No comparable aria2 C++ performance baseline has been recorded. Rust-only
  benchmark results are regression evidence, not proof of superiority.

## Latest SFTP Checkpoint

The local SFTP fixture uses a real SSH handshake and SFTP v3 channel, not a
mocked client transport. It establishes the password, original SHA-1 host-key
pinning, missing-file, full-download, and resume behavior recorded in the
matrix above:

~~~text
cargo test -p aria2-protocol --lib --features sftp -- --test-threads=1
  137 passed, 0 failed
cargo test -p aria2-core --lib engine::sftp_download_command::tests --features sftp -- --test-threads=1
  16 passed, 0 failed
cargo test -p aria2-core --test test_e2e_sftp_download --features sftp -- --test-threads=1
  6 passed, 0 failed
cargo clippy -p aria2-protocol --lib --features sftp -- -D warnings
  PASS
cargo clippy -p aria2-core --lib --features sftp -- -D warnings
  PASS
cargo clippy -p aria2-core --test test_e2e_sftp_download --features sftp -- -D warnings
  PASS
~~~

This is local-server compatibility evidence only. It does not establish
interoperability with OpenSSH or other third-party SFTP servers, public-key
authentication, or the complete original SFTP error and extension matrix.

## Latest RPC Wire Checkpoint

The WebSocket adapter was compared with
`aria2_original/src/WebSocketSession.cc`: the original passes every
non-control frame, including binary frames, to its JSON-RPC parser and returns
text JSON-RPC responses. `aria2-rpc/src/server/ws_session.rs` now preserves
that behavior by dispatching text and binary payload bytes through the same
wire parser. The wildcard CORS route was also checked against
`HttpServer::feedResponse`: with `rpc-allow-origin-all=true`, a normal RPC
response emits `Access-Control-Allow-Origin: *` even without a request
`Origin` header. The existing Rust behavior already matched, so the new E2E
is regression evidence rather than a CORS implementation change.

The same original WebSocket source keeps `rpc-max-request-size` inside its
incremental JSON parser: an oversized non-control message yields JSON-RPC
`-32700` and the session remains usable. The Rust upgrade routes previously
applied this option as an Axum frame/message cap, which reset the socket before
an aria2 response could be sent. `/jsonrpc` passes the logical limit into the
shared parser adapter. A lower transport-level default
limit remains a Rust implementation safety ceiling; it is separate from the
aria2 option and does not affect the covered 1 KiB compatibility case.

For HTTP, `aria2_original` checks an authenticated non-WebSocket request's
declared `Content-Length` before scheduling its body parser. When it exceeds
`rpc-max-request-size`, the server silently drops the connection rather than
writing an HTTP or JSON-RPC error. The process E2E verifies the same Rust
behavior at 1 KiB and separately verifies that failed Basic authentication
still returns its original `401` challenge before this close path. The HTTPS
target uses the repository Rustls certificate fixture to make a real TLS
connection and complete `aria2.getVersion`; it is not only configuration
coverage.

All `aria2-rpc` all-feature test targets completed in the aggregate command,
for 402 passed and 0 failed: 224 library, 18 integration, 55 all-method E2E, 46 HTTP/WebSocket/XML-RPC
route E2E, 5 server, 4 HTTPS, 31 mock-server, 9 header/progress, and 10 stress
tests. `cargo fmt --all -- --check` and
`cargo clippy -p aria2-rpc --all-targets --all-features -- -D warnings` also
passed.

Three process-level public-client tests now supplement the in-process route
suite. `aria2/tests/e2e_arianng_rpc_client.rs` verifies AriaNg's real
`system.multicall` refresh shape with per-subcall `token:` values, nested
results, stringified statistics, and graceful shutdown.
`aria2/tests/e2e_websocket_rpc_client.rs` verifies a live WebSocket
`/jsonrpc` client can authenticate, correlate request IDs, and receive
`aria2.onDownloadStart` and `aria2.onDownloadStop` notifications with the
same GID on one connection. It also verifies an oversized request returns
`-32700` without disconnecting the client, and that a cleanly closed client
can reconnect and receive a later `onDownloadStart` notification.
`aria2/tests/e2e_xmlrpc_client.rs` verifies the live XML-RPC `/rpc` adapter
accepts the leading `token:` parameter, returns `text/xml` method responses,
preserves string statistics, and shuts down cleanly. The latest current-tree
commands passed:

~~~text
cargo test -p aria2 --test e2e_arianng_rpc_client --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2 --test e2e_websocket_rpc_client --all-features -- --test-threads=1
  3 passed, 0 failed
cargo test -p aria2-rpc --test test_e2e_http_server --all-features -- --test-threads=1
  46 passed, 0 failed
cargo test -p aria2-rpc --test test_https_rpc --all-features -- --test-threads=1
  4 passed, 0 failed
cargo test -p aria2-rpc --lib websocket::tests --all-features -- --test-threads=1
  27 passed, 0 failed
cargo test -p aria2 --test e2e_xmlrpc_client --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-core --lib config::option::registry::tests --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2-rpc --lib handlers::handler_tests::test_get_global_option_uses_original_wire_visibility_not_help_visibility --all-features -- --test-threads=1
  1 passed, 0 failed
cargo test -p aria2 --test e2e_rpc_config_inheritance --all-features -j 1 -- --test-threads=1
  5 passed, 0 failed
cargo test -p aria2 --test e2e_rpc_request_limits --all-features -- --test-threads=1
  2 passed, 0 failed
cargo clippy -p aria2 --all-targets --all-features -- -D warnings
  PASS
cargo fmt --all -- --check
  PASS
~~~

The config-inheritance regression starts a live `aria2c` five times: with a
CLI `--dir` value, with `aria2.changeGlobalOption({"dir": ...})`, with a
CLI-created active task followed by that global mutation, and with a
CLI-created task that is force-removed after an applied `changeOption`, plus
one process configured with the Rust-only `--enable-utp=true` option. The
first two prove that a later JSON-RPC `aria2.addUri` inherits the expected
directory and `tellStatus` exposes the corresponding full file path. The last
two task-lifecycle runs prove that `getOption` retains the CLI task's creation-time directory both
while active and after it is stored as a stopped result, including an already
applied task override. The fifth process proves that the public
`getGlobalOption` response includes the C++ hidden `dns-timeout=30`, does not
synthesize the C++ `NO_DEFAULT_VALUE` preference `enable-async-dns6`, reports
that preference after the original `changeGlobalOption` path explicitly
defines it, and does not expose either uTP extension name. Focused
registry/RPC tests additionally cover `rpc-secret` and an unregistered
internal key. This confirms one
canonical global state for new tasks and per-task state across the
live-to-stopped lifecycle; it does not establish every option's changeability,
default, or full original-client interoperability.

The notification publisher now gives each WebSocket connection a unique
scoped subscription. Its receiver and bookkeeping entry share one RAII
lifetime, so a normal disconnect or task cancellation removes the entry just
as `aria2_original` removes a `WebSocketSession`. Public non-WebSocket callers
keep the established explicit `subscribe` / `unsubscribe` interface. The
focused unit regression proves both event delivery and automatic cleanup;
the process regression proves one client can close, reconnect, and receive a
new lifecycle event.

This checkpoint strengthens source-backed and real-client wire compatibility
only. RPC and WebSocket remain `PARTIAL` until original browser extensions
and a broader original-client matrix, notification ordering and broader
reconnect behavior, complete XML-RPC method/error coverage, and the remaining
control-frame combinations have reproducible end-to-end evidence.

## Latest File Allocation Checkpoint

The process-wide file-allocation worker now retains its own `Arc` clone while
`shared()` returns the canonical manager to callers. This fixes the
all-features move-after-capture compile failure without changing queue or
worker lifecycle semantics. The following focused checks passed:

~~~text
cargo check -p aria2-core --all-features                                      PASS
cargo test -p aria2-core --lib filesystem::file_allocation_man::tests --all-features -- --test-threads=1
  16 passed, 0 failed
~~~

## Architecture And Duplication Register

The current refactoring uses these module seams and records the remaining
consolidation work explicitly:

| Concern | Canonical seam | Decision / next action |
| --- | --- | --- |
| Engine command creation | `aria2-core/src/engine/task_spawner.rs` | Keep protocol selection and construction behind this deep module; the engine loop only owns lifecycle accounting and admission. |
| Global bandwidth limits | `RateLimiter` shared through `Arc` | One token-bucket state is shared by active and future commands; RPC updates also refresh `RequestGroupMan`'s reporting snapshot. |
| WebSocket connection lifecycle | `EventPublisher::subscribe_scoped` / `ScopedSubscription` | Each live WebSocket connection receives a unique broadcast registration. Rust RAII removes its bookkeeping entry whenever the receiver task exits, while existing public callers retain explicit `subscribe` / `unsubscribe`. This matches the original session manager's add/remove lifecycle without making the wire adapters own shared publisher state. |
| DHT | `aria2-protocol/src/bittorrent/dht/` | Keep the protocol crate as the canonical implementation; do not revive the unexported duplicate core tree. |
| RPC/CLI option parsing | `aria2-core/src/config/option/registry.rs`, `config/option/types.rs`, `request/request_group/options.rs`, `config/runtime.rs`, and `request/request_group/options_ops.rs` | Keep original runtime changeability, initial-request eligibility, enum choices, typed string/size/integer/boolean parsing, and cumulative `index-out` parsing in core. `OptionRegistry::parse_rpc_value` validates transport values, `DownloadOptions::try_from_rpc_options` validates task creation, and `project_initial_options` encodes the original `setInitialOption(true)` contract for task snapshots; RPC handlers only normalize transport values and map parse failures to aria2 execution errors. BT execution consumes the shared `parse_index_out` result and applies it to its output-path views. Do not add another option whitelist or parser in the RPC crate; JSON-RPC, XML-RPC, WebSocket, CLI, and config adapters must still preserve their distinct original wire parsing, error, and connection behavior. |
| Request-group identity lookup | `aria2-core/src/request/request_group_man/mod.rs` | Keep `active` and `reserved` as scheduling stores, but use one canonical GID index for all non-terminal groups. Active/reserved movement does not remove the index; terminal demotion/removal does. RPC, C API, session snapshots, and status lookup therefore share one stable identity seam without exposing the internal storage choice. Query snapshots preserve active-first and reserved FIFO order, then append canonical-only groups observed during a transfer window. |
| Resume policy | `aria2-core/src/engine/download_command/execute.rs` and `request/request_group/control_ops.rs` | Keep HTTP response interpretation in `SequentialDownloader`; command-level URI selection, atomic resume-failure accumulation, and fresh-download fallback stay in `DownloadCommand`. Do not make RPC or protocol adapters reinterpret `always-resume` or `max-resume-failure-tries`. |
| HTTP/FTP transport | core orchestration plus protocol transport | Existing layers are useful adapters but are not yet one canonical implementation; remove pass-through duplication only after behavior comparison and live interoperability coverage. |
| FTP ownership | `aria2-core/src/engine/ftp_download_command/` is the only production FTP/FTPS path; `aria2-core/src/ftp/connection/negotiation/` and `aria2-protocol/src/ftp/` have no core production callers | Treat the engine path as the current behavioral reference. The target is one deep core FTP transport seam, but `FtpClient`, `FtpNegotiator`, and the standalone protocol client must not be deleted or merged until their public Rust API and behavior are covered by replacement tests. |
| Integrity | core streaming/control-file paths plus Metalink verifier | Preserve separate algorithm-specific adapters for now; unify lifecycle callbacks after the remaining TODO/no-op paths have regression coverage. |

## Acceptance Gate

The external compatibility boundary is strict: every public surface supported
by `aria2_original` must match it, including CLI/configuration, RPC/JSON-RPC/
XML-RPC/WebSocket wire shapes, authentication, parameters, errors, HTTP status
codes, session files, notifications, and observable lifecycle behavior. This
is the compatibility requirement for existing original clients and browser
extensions. Internal Rust architecture and performance are free to improve
only behind that boundary; extensions are evaluated separately and must be
additive.

The migration is not complete until the matrix is backed by reproducible
tests for default, BitTorrent, Metalink, SFTP, RPC, CLI, session/resume, and
binding workflows on the supported platforms. A green focused test or a
completed source comparison must not be reported as workspace all pass.

Performance claims also require a recorded benchmark protocol and comparable
aria2 C measurements. Rust-only benchmark results are regression evidence,
not proof of outperforming the original.
