# aria2-rust User Guide

This guide covers the most common commands, configuration options, session
management, and configuration recovery workflows.

## Basic Downloads

```text
# Download one HTTP/HTTPS file
aria2c https://example.com/file.zip

# Choose the output directory and filename
aria2c --dir=downloads --out=file.zip https://example.com/file.zip

# Use multiple connections for one file
aria2c --split=4 --max-connection-per-server=4 https://example.com/large.iso

# Resume an interrupted download
aria2c --continue=true https://example.com/file.zip

# Download all URIs listed in a text file
aria2c --input-file=urls.txt
```

The command name may be `aria2c`, `aria2c.exe`, or a path to the executable.
The `-d`, `-o`, `-s`, `-x`, `-c`, `-i`, and `-q` short forms are also
supported.

## BitTorrent and Metalink

```text
# Torrent file
aria2c file.torrent

# Magnet URI
aria2c "magnet:?xt=urn:btih:..."

# Metalink file
aria2c file.metalink
```

Common BitTorrent settings include `enable-dht`, `enable-dht6`,
`bt-enable-lpd`, `bt-force-encryption`, `seed-ratio`, and `seed-time`.
Set values explicitly in a configuration file, for example
`bt-force-encryption=true`.

## Configuration Files

A configuration file contains one option per line:

```ini
dir=downloads
continue=true
split=4
max-connection-per-server=4
check-certificate=true
```

Blank lines and lines beginning with `#` or `;` are comments. Known Boolean
options may be written as either `quiet=true` or simply `quiet`. Unknown lines
are ignored with a warning. A known option that is not implemented by the
current runtime is also ignored in a configuration file, which allows a
configuration left by an older aria2 build to remain usable. Invalid values
for supported options are reported with the option and source line.

Templates are available in [`examples/configs/`](../examples/configs/):

- [`minimal.conf`](../examples/configs/minimal.conf)
- [`basic.conf`](../examples/configs/basic.conf)
- [`advanced.conf`](../examples/configs/advanced.conf)
- [`bittorrent.conf`](../examples/configs/bittorrent.conf)
- [`windows.conf`](../examples/configs/windows.conf)

Load a configuration explicitly:

```text
aria2c --conf-path=aria2.conf https://example.com/file.zip
```

When a relative path is used, it is resolved from the process working
directory. Keep the configuration, session, log, PID, and DHT files in the
same working directory when using relative paths.

## Windows Example

From the directory containing `aria2c.exe` and `aria2.conf`:

```powershell
Set-Location C:\Apps\Aria2
New-Item -ItemType Directory -Force downloads | Out-Null
.\aria2c.exe --conf-path=aria2.conf --check-config
.\aria2c.exe --conf-path=aria2.conf
```

The Windows template uses relative paths such as `downloads`,
`aria2.session`, `aria2.log`, `aria2.pid`, `dht.dat`, and `dht6.dat`. This
avoids embedding an installation directory in a distributed configuration.

## RPC and Daemon Mode

A minimal local RPC configuration is:

```ini
enable-rpc=true
rpc-listen-port=6800
rpc-secret=replace-with-a-long-random-token
daemon=true
```

Start it with:

```text
aria2c --conf-path=aria2.conf
```

Keep `rpc-listen-all` disabled unless remote access is required. If remote
access is enabled, use `rpc-secret` and restrict access with a firewall.
`pid-file=aria2.pid` allows daemon startup checks to detect an existing
instance.

## Session Files

Use one session file for restoring unfinished tasks and saving them on exit:

```ini
input-file=aria2.session
save-session=aria2.session
save-session-interval=60
```

The session file is not a URI list. Do not edit it while aria2-rust is
running. The `.aria2` control files next to downloaded files are separate
resume metadata.

## Check, Repair, and Reset

Check a configuration without starting a download engine:

```text
aria2c --conf-path=aria2.conf --check-config
```

Repair only invalid entries:

```text
aria2c --conf-path=aria2.conf --repair-config
```

Repair creates a non-overwriting backup such as `aria2.conf.bak` or
`aria2.conf.bak.1`, then comments out each invalid line and leaves valid and
legacy entries unchanged. Review the repaired file before starting downloads.

Reset the file to the built-in defaults:

```text
aria2c --conf-path=aria2.conf --reset-config
```

Reset also creates a non-overwriting backup. It is appropriate when the file
has accumulated conflicting settings and starting from defaults is preferable
to repairing individual lines. These commands require an explicit
`--conf-path` so that a maintenance operation cannot unexpectedly target a
different default file.

## Common Options

| Option | Purpose | Example |
| --- | --- | --- |
| `--dir`, `-d` | Download directory | `--dir=downloads` |
| `--out`, `-o` | Output filename | `--out=archive.zip` |
| `--split`, `-s` | Concurrent requests for one download | `--split=4` |
| `--max-connection-per-server`, `-x` | Per-server request limit | `--max-connection-per-server=4` |
| `--continue`, `-c` | Resume partial files | `--continue=true` |
| `--max-download-limit` | Per-download speed limit | `--max-download-limit=2M` |
| `--max-overall-download-limit` | Global speed limit | `--max-overall-download-limit=10M` |
| `--timeout` | Network timeout in seconds | `--timeout=60` |
| `--check-certificate` | Verify HTTPS certificates | `--check-certificate=true` |
| `--quiet`, `-q` | Reduce console output | `--quiet=true` |
| `--input-file`, `-i` | Load a session/URI input file | `--input-file=aria2.session` |
| `--save-session` | Save unfinished tasks | `--save-session=aria2.session` |
| `--conf-path` | Select a configuration file | `--conf-path=aria2.conf` |

Use `aria2c --help` for the complete option list.
