# HLS / M3U8 Downloads

aria2 supports HTTP(S) URLs ending in `.m3u8` as an intentional extension to
the protocols supported by original aria2. The engine parses the playlist,
applies the task's HTTP policy, and delegates media retrieval and muxing to a
user-installed FFmpeg executable.

## FFmpeg requirement

aria2 does not bundle, download, or install FFmpeg. Install it yourself and
make `ffmpeg` available on `PATH`, or set `ARIA2C_FFMPEG` to the absolute path
of the executable:

```bash
export ARIA2C_FFMPEG=/usr/local/bin/ffmpeg
aria2c 'https://media.example.com/live/stream.m3u8'
```

On Windows:

```powershell
$env:ARIA2C_FFMPEG = 'C:\Tools\ffmpeg\bin\ffmpeg.exe'
aria2c.exe 'https://media.example.com/live/stream.m3u8'
```

If FFmpeg cannot be started, aria2 reports the configured path and explains
how to install it or set `ARIA2C_FFMPEG`.

## Output

An inferred `.m3u8` filename becomes a `.mp4` file. An explicit output name
may use `.mp4`, `.mkv`, `.ts`, `.mov`, `.webm`, `.avi`, `.flv`, `.mp3`, `.m4a`,
or `.wav`; other extensions are rejected. FFmpeg writes to a staging file in
the destination directory and aria2 renames it only after a successful exit,
so a failed transfer does not replace the requested output file.

```bash
aria2c -o concert.mp4 'https://media.example.com/hls/master.m3u8'
```

JSON-RPC and XML-RPC tasks accept the same URL:

```json
{
  "jsonrpc": "2.0",
  "method": "aria2.addUri",
  "id": 1,
  "params": [["https://media.example.com/hls/master.m3u8"]]
}
```

## Supported playlist behavior

- Master playlists: the highest `AVERAGE-BANDWIDTH` variant is selected;
  `BANDWIDTH` is the fallback tie-break.
- Media playlists: VOD and live playlists, `EXT-X-KEY` AES-128, `EXT-X-MAP`,
  `EXT-X-BYTERANGE`, discontinuities, and gaps are passed to FFmpeg's HLS
  demuxer.
- Alternate audio and subtitle renditions follow FFmpeg's default selection.
- Headers, `User-Agent`, `Referer`, cookies, proxy settings, and basic
  credentials from the aria2 task are forwarded to FFmpeg.
- `--dry-run` validates the playlist without starting FFmpeg.

For a live playlist without `EXT-X-ENDLIST`, set `--timeout=N` to record at
most N seconds. Without it, the transfer continues until it is paused,
stopped, or removed. Pausing or cancelling stops FFmpeg and discards the
staging file. Live capture is staged as MPEG-TS so an interrupted recording
remains decodable, then remuxed into the requested output container.

## Limitations

- HLS cannot resume at arbitrary byte offsets because the output is a muxed
  container. A cancelled transfer must be restarted from the playlist.
- The selected variant and rendition cannot be overridden by an HLS-specific
  aria2 option.
- `--max-download-limit` and the process-wide aria2 limiter cannot govern
  FFmpeg's sockets.
- Streams are copied without video or audio re-encoding. Some codec and
  container combinations are not muxable with `-c copy`; use a compatible
  output extension such as `.mkv` or `.ts`.
- Detection is based on an HTTP(S) URL whose path ends in `.m3u8`. A playlist
  served from an extensionless URL is treated as an ordinary HTTP file.
- HLS output is written to a local filesystem path; SMB targets are outside
  this implementation.

See [compatibility-status.md](compatibility-status.md) for verification
evidence and the current scope.
