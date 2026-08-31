# aria2-rust 0.3.4 Binary Release Notes

This document describes the `aria2-rust 0.3.4` binary distribution, supported
targets, artifact names, build properties, verification steps, installation
and upgrade procedure, compatibility boundaries, and release validation. It
applies to the `aria2c` executable published in the GitHub Release. It is not a
claim of Rust library API, C API, or C++ ABI compatibility.

## 1. Release Identity

| Item | Value |
| --- | --- |
| Product | `aria2-rust` |
| Command-line program | `aria2c` |
| Binary version | `0.3.4` |
| Release tag | `v0.3.4` |
| Binary version source | `aria2/Cargo.toml` |
| Release branch | `master` |
| Release mechanism | GitHub Actions Release workflow |

The version reported by `aria2c --version`, the startup banner, RPC
`aria2.getVersion`, and the default HTTP and BitTorrent identity values comes
from the binary package `aria2`. The `aria2-core`, `aria2-protocol`, and
`aria2-rpc` packages are independently versioned library packages. Their
package versions are not binary file names and are not Release tags.

## 2. User-visible Changes

The 0.3.4 binary includes the following functional areas:

- Improved process shutdown, timed halt, dependency cleanup, and lifecycle
  handling to reduce leftover work and resources during exit.
- Improved disk-cache eviction, range coalescing, overlap handling, cache
  flushing, and writer I/O statistics.
- Improved download throttling and release-build configuration while keeping
  blocking filesystem work away from the async reactor.
- Stronger HTTP Range response, BitTorrent message, and Bencode container
  validation, including explicit container element limits.
- Improved BitTorrent shutdown, endgame, tracker, PEX, piece-state, checkpoint,
  and recovery edge-case handling, with additional real-download regression
  coverage.
- Improved configuration parsing and runtime option behavior, including the
  distinction between a one-shot CLI download and an explicitly requested RPC
  service.
- Added or improved lightweight background update checks and the
  `check-update` command.
- Continued maintenance of the HTTP, BitTorrent, RPC, Metalink, and SFTP
  feature boundaries. The default binary enables HTTP, BitTorrent, and RPC;
  Metalink and SFTP remain explicit build features.

The release also contains internal module cleanup, tests, and documentation
updates. These notes describe behavior implemented by this Rust project. They
do not claim complete semantic, protocol, or ABI equivalence with
`aria2_original`.

## 3. Official Binary Artifacts

The Release workflow builds these targets after a push to `master`:

| Platform | Rust target | Artifact | Format |
| --- | --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `aria2-x86_64-linux.tar.gz` | tar + gzip |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `aria2-x86_64-windows.zip` | ZIP |
| macOS Apple Silicon | `aarch64-apple-darwin` | `aria2-aarch64-macos.tar.gz` | tar + gzip |
| macOS Intel | `x86_64-apple-darwin` | `aria2-x86_64-macos.tar.gz` | tar + gzip |

Each archive currently contains the platform-specific `aria2c` executable:
`aria2c.exe` on Windows and `aria2c` on Linux and macOS. Release builds use
optimization level 3, thin LTO, `panic=abort`, and symbol stripping. The
official executable is therefore suitable for deployment but does not contain
debug symbols.

The Windows ZIP is accompanied by
`aria2-x86_64-windows.zip.sha256`. Linux and macOS do not currently receive a
separate checksum asset from the Release workflow; calculate a local checksum
after downloading those archives.

## 4. Verify Downloads

### Windows PowerShell

```powershell
$file = '.\\aria2-x86_64-windows.zip'
(Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
Get-Content '.\\aria2-x86_64-windows.zip.sha256'
```

Compare the 64-character SHA-256 values. The checksum file name must match the
downloaded archive. Do not extract or replace an installation when the values
do not match.

### Linux

```bash
sha256sum aria2-x86_64-linux.tar.gz
tar -xzf aria2-x86_64-linux.tar.gz
./aria2c --version
```

### macOS

```bash
shasum -a 256 aria2-aarch64-macos.tar.gz
tar -xzf aria2-aarch64-macos.tar.gz
./aria2c --version
```

If the operating system blocks first launch, verify that the file came from the
trusted GitHub Release before applying the platform security approval. Do not
treat an unverified third-party repack as an official artifact.

## 5. Installation and Upgrade

### Direct installation

Stop the old process before replacing its executable. Extract the matching
archive and place `aria2c` or `aria2c.exe` in a directory on `PATH`, or invoke
it by an absolute path. Existing configuration files, download directories,
session files, logs, and control files may be retained after a verified upgrade.

```bash
aria2c --version
aria2c --help
```

### Build from source

```bash
cargo build -p aria2 --release
```

The output is `target/release/aria2c`, or `target/release/aria2c.exe` on
Windows. The default features include HTTP, BitTorrent, and RPC. Build
Metalink or SFTP explicitly when required:

```bash
cargo build -p aria2 --release --features "metalink,sftp"
```

A source build is not identical to the official Release binary. Compiler
version, enabled features, linker, system libraries, and build flags may differ.

### Scoop

Windows users can use the repository manifest:

```powershell
scoop install https://raw.githubusercontent.com/balovess/aria2_rust/master/scoop/aria2-rust.json
scoop update aria2-rust
```

The manifest depends on the Windows ZIP and its SHA-256 asset in the GitHub
Release. A Scoop update is not complete when either the Release or checksum
asset is unavailable.

## 6. Upgrade and Compatibility Notes

- Back up configuration, `.aria2` control files, session files, and download
  directories before upgrading.
- Do not replace the executable while an old process is still writing session
  or payload state.
- Before reusing an RPC client, verify `aria2c --version`, RPC
  `aria2.getVersion`, and the methods and options required by that client.
- The Rust-owned A2CF BitTorrent checkpoint format is not a complete binary
  `.aria2` interoperability promise with `aria2_original`.
- The C API exposes a Rust-owned opaque-handle interface. It does not claim
  binary compatibility with the original C++ classes, STL, or original ABI.
- Runtime behavior depends on filesystem, permissions, TLS backend, and host
  security policy. Passing the official platform matrix does not validate every
  third-party filesystem or system configuration.

## 7. Release Validation Boundary

Before publishing a binary Release, verify all of the following:

1. The `dev` CI lint/format job and Linux, Windows, and macOS test jobs pass.
2. The `master` Release workflow reads the version from `aria2/Cargo.toml` and
   builds all four platform artifacts.
3. The Windows ZIP, checksum asset, Release tag, and reported binary version
   agree.
4. Running `aria2c --version` after download reports `0.3.4`.
5. The Scoop manifest URL and SHA-256 match the GitHub Release when Scoop is
   supported for the release.

CI success proves only the checks defined by the repository workflow. It does
not automatically prove every package-manager integration, third-party RPC
client, original aria2 interoperability scenario, or cross-platform ABI.
