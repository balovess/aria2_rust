# aria2-rust Troubleshooting

## The program does not start

Confirm the binary and working directory first:

```text
aria2c --version
aria2c --help
```

Build from source with `cargo build -p aria2 --release`. The Windows executable is `aria2c.exe`. Relative paths for ports, logs, sessions, and downloads are resolved from the current working directory.

## Configuration errors or ignored settings

```text
aria2c --conf-path=aria2.conf --check-config
```

Use one `name=value` entry per line and do not write the `--` prefix in a configuration file. Boolean values are `true` or `false`. Precedence is built-in defaults < environment < configuration file < command line, so CLI values override the file. Unknown and currently unsupported compatibility options produce warnings; follow the current `aria2c --help` output.

To repair invalid entries:

```text
aria2c --conf-path=aria2.conf --repair-config
```

## RPC connection failure

Confirm that `enable-rpc=true` is set and check the address and port:

```text
aria2c --conf-path=aria2.conf
```

The default endpoint is `http://127.0.0.1:6800/jsonrpc`. When `rpc-secret` is configured, pass `token:<secret>` as the first `params` item. Authentication failure is separate from download-task failure. Remote access also requires a listening address and an appropriate firewall rule.

## HTTPS or CORS failure

`rpc-secure=true` requires PEM-format `rpc-certificate` and `rpc-private-key`. For browser clients, set `rpc-cors-domain` to the actual page origin. Use `rpc-allow-origin-all=true` only in a controlled environment.

## Zero speed or failed downloads

Inspect `status`, `errorCode`, and `errorMessage` through `aria2.tellStatus` or console output. Common causes include an unreachable URL, failed TLS verification, incorrect proxy settings, a timeout that is too short, or a server that rejects Range requests. Do not increase `split` first; verify a single-connection download, then tune `split` and `max-connection-per-server`.

## BitTorrent, Metalink, or SFTP is unavailable

Use `aria2.getVersion` to inspect `enabledFeatures`. The default build includes BitTorrent but not Metalink or SFTP. Rebuild with `--features "metalink,sftp"` when required. Use a feature-specific RPC method only when the feature is enabled and the method appears in `system.listMethods`.

## Resume or session restoration problems

The `.aria2` control file stores piece-resume data for one download. A session file stores the task list; they are different files. Stop the program before inspecting or moving them, and do not edit a session file while aria2c is running. Use `save-session` to save unfinished tasks and `input-file` to read them on the next start.
