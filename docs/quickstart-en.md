# aria2-rust Quick Start

## 1. Get the program

The repository currently supports building from Rust source. Install Rust stable and Cargo, then run:

```text
cargo build -p aria2 --release
```

The binary is `target/release/aria2c` (`target/release/aria2c.exe` on Windows). The default build includes HTTP, BitTorrent, and RPC. Enable Metalink or SFTP explicitly when needed:

```text
cargo build -p aria2 --release --features "metalink,sftp"
```

## 2. First download

```text
aria2c https://example.com/file.zip
aria2c --dir=downloads --out=file.zip https://example.com/file.zip
aria2c --continue=true https://example.com/file.zip
```

BitTorrent accepts `.torrent` files and Magnet URIs. Metalink is available only in builds with the `metalink` feature enabled.

## 3. Use a configuration file

```text
aria2c --conf-path=aria2.conf https://example.com/file.zip
aria2c --conf-path=aria2.conf --check-config
```

Minimal configuration:

```ini
dir=downloads
continue=true
split=4
max-connection-per-server=4
```

See [`configuration-guide-en.md`](configuration-guide-en.md) for the complete option reference. Ready-to-use templates are in [`../examples/configs/`](../examples/configs/).

## 4. Enable RPC

```ini
enable-rpc=true
rpc-listen-address=127.0.0.1
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
```

```text
aria2c --conf-path=aria2.conf
```

The default RPC URL is `http://127.0.0.1:6800/jsonrpc`. See [`rpc-guide-en.md`](rpc-guide-en.md) for methods, authentication, and HTTPS.

## 5. Common next steps

- Resume downloads: set `continue=true` and keep the `.aria2` control file beside the download.
- Restore a session: set both `input-file=aria2.session` and `save-session=aria2.session`.
- Validate configuration: use `--check-config` before starting downloads.
- List all options: run `aria2c --help`.
