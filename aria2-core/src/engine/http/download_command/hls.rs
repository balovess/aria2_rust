//! HLS/M3U8 transfer adapter.
//!
//! Playlist parsing stays in `aria2-protocol`. Media retrieval, decryption,
//! rendition selection, and container muxing are delegated to a user-installed
//! FFmpeg executable so that the engine does not duplicate the HLS protocol.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use aria2_protocol::hls::{MediaPlaylist, Playlist};
use futures::StreamExt;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, warn};
use url::Url;

use crate::engine::active_output_registry::{OutputPathPolicy, global_registry};
use crate::engine::http::auth;
use crate::error::{Aria2Error, Result};
use crate::filesystem::durability;
use crate::request::request_group::DownloadOptions;
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

const MAX_PLAYLIST_BYTES: usize = 4 * 1024 * 1024;
const MAX_NESTED_PLAYLISTS: usize = 8;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const STDERR_LIMIT: usize = 64 * 1024;

const FFMPEG_ENV: &str = "ARIA2C_FFMPEG";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputFormat {
    extension: &'static str,
    muxer: &'static str,
}

const MP4: OutputFormat = OutputFormat {
    extension: "mp4",
    muxer: "mp4",
};

const MPEGTS: OutputFormat = OutputFormat {
    extension: "ts",
    muxer: "mpegts",
};

struct HlsPlan {
    playlist_url: Url,
    media: MediaPlaylist,
    selected_bandwidth: Option<u64>,
}

impl HlsPlan {
    fn is_live(&self) -> bool {
        self.media.is_live()
    }

    /// A bandwidth-derived estimate is only available for master playlists.
    /// Media playlists leave the total size unknown until FFmpeg finishes.
    fn estimated_total_length(&self) -> u64 {
        self.selected_bandwidth
            .map(|bits_per_second| {
                ((bits_per_second as f64 * self.media.duration_seconds()) / 8.0).floor() as u64
            })
            .unwrap_or(0)
    }
}

impl DownloadCommand {
    pub(super) async fn execute_hls_playlist(&mut self, uri: &str) -> Result<()> {
        self.check_cancelled()?;

        let options = self.group.recover().options_arc();
        let program = resolve_ffmpeg_program();
        let program_label = program.to_string_lossy().into_owned();
        let plan = self.prepare_hls_plan(uri).await?;

        let estimated_length = plan.estimated_total_length();
        if estimated_length > 0 {
            self.group.recover().set_total_length(estimated_length);
        }

        if options.dry_run {
            self.completed_bytes = plan.estimated_total_length();
            let mut group = self.group.recover_mut();
            group.set_total_length(self.completed_bytes);
            group.update_progress(self.completed_bytes);
            group.set_checksum_verified(true);
            group.complete()?;
            self.completed = true;
            return Ok(());
        }

        let (output_path, format) = hls_output_path(&self.output_path)?;
        self.output_path = output_path;
        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                Aria2Error::FileIo(format!("Failed to create output directory: {error}"))
            })?;
        }
        if !self.output_path_resolved {
            self.output_path = global_registry()
                .resolve_with_policy(
                    &self.output_path,
                    OutputPathPolicy {
                        allow_overwrite: options.allow_overwrite,
                        auto_file_renaming: options.auto_file_renaming,
                        continue_download: false,
                        check_integrity: self.check_integrity,
                        total_length: None,
                    },
                )
                .await?;
            self.output_path_resolved = true;
        }
        self.publish_output_path();

        let parent = self.output_path.parent().unwrap_or_else(|| Path::new("."));
        let staging_dir = tempfile::Builder::new()
            .prefix(".aria2-hls-")
            .tempdir_in(parent)
            .map_err(|error| {
                Aria2Error::FileIo(format!("Failed to create HLS staging directory: {error}"))
            })?;
        let staging_format = if plan.is_live() { MPEGTS } else { format };
        let staged_output = staging_dir
            .path()
            .join(format!("payload.{}", staging_format.extension));

        let headers = self.hls_request_headers(&options, &plan.playlist_url)?;
        let command = build_ffmpeg_command(
            &program,
            &plan,
            &staged_output,
            staging_format,
            &options,
            &headers,
        );
        self.run_ffmpeg(
            command,
            &program_label,
            &staged_output,
            &options,
            plan.is_live(),
        )
        .await?;

        let publishable_stage = if plan.is_live() {
            let final_stage = staging_dir
                .path()
                .join(format!("remuxed.{}", format.extension));
            remux_live_capture(
                &program,
                &program_label,
                &staged_output,
                &final_stage,
                format,
            )
            .await?;
            final_stage
        } else {
            staged_output
        };
        self.publish_staged_output(&publishable_stage).await?;
        Ok(())
    }

    async fn prepare_hls_plan(&self, uri: &str) -> Result<HlsPlan> {
        let mut current_url = Url::parse(uri).map_err(|error| {
            Aria2Error::Fatal(crate::error::FatalError::Config(error.to_string()))
        })?;
        let mut visited = HashSet::new();
        let mut selected_bandwidth = None;

        for _ in 0..MAX_NESTED_PLAYLISTS {
            self.check_cancelled()?;
            if !visited.insert(current_url.clone()) {
                return Err(playlist_error("HLS playlist URI cycle detected"));
            }

            let prepared = self
                .send_get_with_redirects(current_url.as_str())
                .await
                .ok_or_else(|| Aria2Error::Network("Failed to fetch HLS playlist".into()))?;
            if !prepared.response.status().is_success() {
                return Err(Aria2Error::Network(format!(
                    "HLS playlist request returned HTTP {}",
                    prepared.response.status().as_u16()
                )));
            }
            let effective_url = Url::parse(&prepared.effective_uri).map_err(|error| {
                Aria2Error::Fatal(crate::error::FatalError::Config(error.to_string()))
            })?;
            let body = read_bounded_playlist(prepared.response).await?;
            let body = String::from_utf8(body).map_err(|error| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "HLS playlist is not valid UTF-8: {error}"
                )))
            })?;

            match aria2_protocol::hls::parse_playlist(&body, &effective_url)
                .map_err(|error| playlist_error(error.to_string()))?
            {
                Playlist::Media(media) => {
                    self.group
                        .recover()
                        .set_content_type("application/vnd.apple.mpegurl");
                    if media.is_live() && self.group.recover().options().timeout.is_none() {
                        warn!(
                            "Live HLS playlist has no ENDLIST; set --timeout to bound the recording or cancel the download to stop it"
                        );
                    }
                    return Ok(HlsPlan {
                        playlist_url: effective_url,
                        media,
                        selected_bandwidth,
                    });
                }
                Playlist::Master(master) => {
                    let variant = master
                        .best_variant()
                        .ok_or_else(|| playlist_error("HLS master playlist has no variants"))?;
                    selected_bandwidth = Some(variant.bandwidth);
                    debug!(
                        bandwidth = variant.bandwidth,
                        uri = %variant.uri,
                        "Selected HLS variant"
                    );
                    current_url = variant.uri.clone();
                }
            }
        }

        Err(playlist_error(
            "HLS master playlist nesting exceeds 8 levels",
        ))
    }

    fn hls_request_headers(
        &self,
        options: &DownloadOptions,
        url: &Url,
    ) -> Result<Vec<(String, String)>> {
        let mut headers = options.parsed_headers();
        if !has_header(&headers, "Cookie") {
            let cookie = self
                .create_cookie_helper()
                .build_cookie_header_from_url(url);
            if !cookie.is_empty() {
                headers.push(("Cookie".to_owned(), cookie));
            }
        }
        if !has_header(&headers, "Authorization") {
            let context = auth::from_options(options, url.scheme());
            let mut factory = context.factory;
            if let Some(authorization) = factory.resolve_basic_authorization(url, &context.options)
            {
                headers.push(("Authorization".to_owned(), authorization));
            }
        }
        validate_headers(&headers)?;
        Ok(headers)
    }

    async fn run_ffmpeg(
        &mut self,
        mut command: Command,
        program_label: &str,
        staged_output: &Path,
        options: &DownloadOptions,
        enforce_recording_limit: bool,
    ) -> Result<()> {
        let mut child = command
            .spawn()
            .map_err(|error| ffmpeg_spawn_error(program_label, error))?;
        let stderr = child.stderr.take().ok_or_else(|| {
            Aria2Error::DownloadFailed("FFmpeg stderr pipe is unavailable".into())
        })?;
        let stderr_task = tokio::spawn(read_stderr_limited(stderr));
        let started = Instant::now();
        let recording_limit = enforce_recording_limit
            .then_some(options.timeout)
            .flatten()
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);
        let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let status = loop {
            tokio::select! {
                _ = ticker.tick() => {
                    check_hls_group(&self.group)?;
                    if recording_limit.is_some_and(|limit| started.elapsed() >= limit) {
                        stderr_task.abort();
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        debug!("Stopped FFmpeg at the configured --timeout limit");
                        return Ok(());
                    }
                    report_staged_progress(self, staged_output);
                }
                status = child.wait() => {
                    break status.map_err(|error| {
                        Aria2Error::DownloadFailed(format!("Failed to wait for FFmpeg: {error}"))
                    })?;
                }
            }
        };

        let stderr_output = stderr_task.await.unwrap_or_default();
        if !status.success() {
            let detail = stderr_output.trim();
            return Err(Aria2Error::DownloadFailed(if detail.is_empty() {
                format!("FFmpeg exited with {status}")
            } else {
                format!("FFmpeg exited with {status}: {detail}")
            }));
        }
        if !stderr_output.trim().is_empty() {
            debug!(output = %stderr_output.trim(), "FFmpeg warnings");
        }
        report_staged_progress(self, staged_output);
        Ok(())
    }

    async fn publish_staged_output(&mut self, staged_output: &Path) -> Result<()> {
        let length = tokio::fs::metadata(staged_output)
            .await
            .map_err(|error| Aria2Error::FileIo(format!("Failed to inspect HLS output: {error}")))?
            .len();
        if length == 0 {
            return Err(Aria2Error::DownloadFailed(
                "FFmpeg produced an empty HLS output file".into(),
            ));
        }

        if self.output_path.exists() && self.group.recover().options().allow_overwrite {
            tokio::fs::remove_file(&self.output_path)
                .await
                .map_err(|error| {
                    Aria2Error::FileIo(format!(
                        "Failed to replace existing output {}: {error}",
                        self.output_path.display()
                    ))
                })?;
        }
        tokio::fs::rename(staged_output, &self.output_path)
            .await
            .map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to move HLS output to {}: {error}",
                    self.output_path.display()
                ))
            })?;
        durability::sync_parent_directories(&self.output_path).await?;

        self.completed_bytes = length;
        let group = self.group.recover();
        group.set_total_length(length);
        group.update_progress(length);
        group.set_completed_length(length);
        Ok(())
    }
}

fn playlist_error(message: impl Into<String>) -> Aria2Error {
    Aria2Error::Fatal(crate::error::FatalError::Config(message.into()))
}

fn resolve_ffmpeg_program() -> OsString {
    std::env::var_os(FFMPEG_ENV)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("ffmpeg"))
}

fn ffmpeg_spawn_error(program: &str, error: std::io::Error) -> Aria2Error {
    let configured = std::env::var_os(FFMPEG_ENV)
        .filter(|value| !value.is_empty())
        .is_some();
    ffmpeg_spawn_error_for(program, error, configured)
}

fn ffmpeg_spawn_error_for(program: &str, error: std::io::Error, configured: bool) -> Aria2Error {
    let message = if configured {
        format!(
            "Could not start configured FFmpeg program '{program}': {error}. Check {FFMPEG_ENV} and file permissions."
        )
    } else {
        format!(
            "Could not start FFmpeg: {error}. HLS downloads require user-installed FFmpeg available on PATH. Install FFmpeg or set {FFMPEG_ENV} to its absolute path; aria2 does not bundle FFmpeg."
        )
    };
    Aria2Error::Fatal(crate::error::FatalError::Config(message))
}

async fn read_bounded_playlist(response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_PLAYLIST_BYTES)
    {
        return Err(playlist_error("HLS playlist exceeds the 4 MiB limit"));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| Aria2Error::Network(error.to_string()))?;
        if body.len().saturating_add(chunk.len()) > MAX_PLAYLIST_BYTES {
            return Err(playlist_error("HLS playlist exceeds the 4 MiB limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn hls_output_path(path: &Path) -> Result<(PathBuf, OutputFormat)> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let format = match extension.as_str() {
        "" | "m3u8" | "mp4" | "m4v" => MP4,
        "mkv" => OutputFormat {
            extension: "mkv",
            muxer: "matroska",
        },
        "ts" => OutputFormat {
            extension: "ts",
            muxer: "mpegts",
        },
        "mov" => OutputFormat {
            extension: "mov",
            muxer: "mov",
        },
        "webm" => OutputFormat {
            extension: "webm",
            muxer: "webm",
        },
        "avi" => OutputFormat {
            extension: "avi",
            muxer: "avi",
        },
        "flv" => OutputFormat {
            extension: "flv",
            muxer: "flv",
        },
        "mp3" => OutputFormat {
            extension: "mp3",
            muxer: "mp3",
        },
        "m4a" => OutputFormat {
            extension: "m4a",
            muxer: "ipod",
        },
        "wav" => OutputFormat {
            extension: "wav",
            muxer: "wav",
        },
        other => {
            return Err(playlist_error(format!(
                "Unsupported HLS output extension '.{other}'. Use .mp4, .mkv, .ts, .mov, .webm, .avi, .flv, .mp3, .m4a, or .wav"
            )));
        }
    };

    let mut output = path.to_path_buf();
    if extension.is_empty() || extension == "m3u8" {
        output.set_extension(format.extension);
    }
    Ok((output, format))
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(header, _)| header.eq_ignore_ascii_case(name))
}

fn validate_headers(headers: &[(String, String)]) -> Result<()> {
    for (name, value) in headers {
        let invalid_name = name.is_empty()
            || name
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | ':' | ' ' | '\t'));
        if invalid_name {
            return Err(playlist_error(format!(
                "Invalid HTTP header name for HLS request: {name:?}"
            )));
        }
        if value
            .chars()
            .any(|character| matches!(character, '\r' | '\n'))
        {
            return Err(playlist_error(format!(
                "Invalid HTTP header value for HLS request: {name}"
            )));
        }
    }
    Ok(())
}

fn build_ffmpeg_command(
    program: &OsString,
    plan: &HlsPlan,
    staged_output: &Path,
    format: OutputFormat,
    options: &DownloadOptions,
    headers: &[(String, String)],
) -> Command {
    let mut command = Command::new(program);
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-nostats")
        .arg("-loglevel")
        .arg("warning")
        .arg("-allowed_extensions")
        .arg("ALL");

    if let Some(proxy) = options
        .http_proxy_options()
        .proxy_url_for_uri(plan.playlist_url.as_str())
    {
        apply_ffmpeg_proxy(&mut command, &proxy, plan.playlist_url.scheme());
    }
    if !options.check_certificate {
        command.arg("-tls_verify").arg("0");
    }
    if let Some(ca_certificate) = options.ca_certificate.as_deref() {
        command.arg("-ca_file").arg(ca_certificate);
    }
    if let Some(certificate) = options.certificate.as_deref() {
        command.arg("-cert_file").arg(certificate);
    }
    if let Some(key) = options.private_key.as_deref() {
        command.arg("-key_file").arg(key);
    }
    let connect_timeout = options
        .connect_timeout
        .unwrap_or(crate::constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS);
    command
        .arg("-rw_timeout")
        .arg((connect_timeout.saturating_mul(1_000_000)).to_string());
    let user_agent = options
        .http_user_agent()
        .unwrap_or(crate::constants::USER_AGENT);
    command.arg("-user_agent").arg(user_agent);
    if let Some(referer) = options.referer.as_deref() {
        command.arg("-referer").arg(referer);
    }
    if !headers.is_empty() {
        let mut header_lines = String::new();
        for (name, value) in headers {
            use std::fmt::Write as _;
            let _ = writeln!(header_lines, "{name}: {value}\r");
        }
        command.arg("-headers").arg(header_lines);
    }

    command.arg("-i").arg(plan.playlist_url.as_str());
    command
        .arg("-map")
        .arg("0:v?")
        .arg("-map")
        .arg("0:a?")
        .arg("-map")
        .arg("0:s?")
        .arg("-c")
        .arg("copy");

    if format == MP4 {
        command.arg("-c:s").arg("mov_text");
        if uses_mpegts_segments(&plan.media) {
            command.arg("-bsf:a").arg("aac_adtstoasc");
        }
        command.arg("-movflags").arg("+faststart");
    }
    command
        .arg("-f")
        .arg(format.muxer)
        .arg(staged_output)
        .kill_on_drop(true);
    command
}

async fn remux_live_capture(
    program: &OsString,
    program_label: &str,
    capture: &Path,
    destination: &Path,
    format: OutputFormat,
) -> Result<()> {
    let mut command = Command::new(program);
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("warning")
        .arg("-i")
        .arg(capture)
        .arg("-map")
        .arg("0:v?")
        .arg("-map")
        .arg("0:a?")
        .arg("-map")
        .arg("0:s?")
        .arg("-c")
        .arg("copy");
    if format == MP4 {
        command.arg("-c:s").arg("mov_text");
        command.arg("-bsf:a").arg("aac_adtstoasc");
        command.arg("-movflags").arg("+faststart");
    }
    command.arg("-f").arg(format.muxer).arg(destination);

    let mut child = command
        .spawn()
        .map_err(|error| ffmpeg_spawn_error(program_label, error))?;
    let stderr = child.stderr.take().ok_or_else(|| {
        Aria2Error::DownloadFailed("FFmpeg remux stderr pipe is unavailable".into())
    })?;
    let stderr_task = tokio::spawn(read_stderr_limited(stderr));
    let status = child.wait().await.map_err(|error| {
        Aria2Error::DownloadFailed(format!("Failed to wait for FFmpeg remux: {error}"))
    })?;
    let stderr_output = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let detail = stderr_output.trim();
        return Err(Aria2Error::DownloadFailed(format!(
            "FFmpeg failed to mux the live capture with exit status {status}: {detail}"
        )));
    }
    Ok(())
}

fn apply_ffmpeg_proxy(command: &mut Command, proxy: &str, target_scheme: &str) {
    // FFmpeg builds differ in SOCKS support. The HTTP(S) proxy options accept
    // the configured URL and surface FFmpeg's own diagnostic if the build
    // cannot handle its scheme.
    if target_scheme == "https" {
        command.arg("-https_proxy").arg(proxy);
    } else {
        command.arg("-http_proxy").arg(proxy);
    }
}

fn uses_mpegts_segments(playlist: &MediaPlaylist) -> bool {
    playlist
        .segments
        .iter()
        .any(|segment| segment.uri.path().to_ascii_lowercase().ends_with(".ts"))
}

async fn read_stderr_limited(mut stderr: impl AsyncReadExt + Unpin) -> String {
    let mut buffer = vec![0_u8; 8192];
    let mut output = Vec::new();
    while let Ok(count) = stderr.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        if output.len() + count > STDERR_LIMIT {
            output.extend_from_slice(&buffer[..count]);
            let start = output.len() - STDERR_LIMIT;
            output.drain(..start);
            break;
        }
        output.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn report_staged_progress(command: &mut DownloadCommand, staged_output: &Path) {
    let Ok(length) = std::fs::metadata(staged_output).map(|metadata| metadata.len()) else {
        return;
    };
    if length <= command.completed_bytes {
        return;
    }
    command.completed_bytes = length;
    command.group.recover().update_progress(length);
}

fn check_hls_group(
    group: &std::sync::Arc<std::sync::RwLock<crate::request::request_group::RequestGroup>>,
) -> Result<()> {
    let group = group.recover();
    if group.is_removed() {
        return Err(Aria2Error::DownloadFailed(
            "Download cancelled by user".into(),
        ));
    }
    if group.is_paused_flag() {
        return Err(Aria2Error::DownloadFailed("Download paused".into()));
    }
    if group.is_force_halt_requested() || group.is_halt_requested() {
        return Err(Aria2Error::DownloadFailed("Download halted".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::hls::{MediaSegment, PlaylistError};

    fn media_playlist() -> MediaPlaylist {
        MediaPlaylist {
            target_duration: Some(4),
            media_sequence: 0,
            playlist_type: None,
            end_list: true,
            segments: vec![MediaSegment {
                uri: Url::parse("https://media.example.test/seg1.ts").unwrap(),
                duration_seconds: 4.0,
                title: String::new(),
                byte_range: None,
                initialization: None,
                key: None,
                discontinuity: false,
                gap: false,
            }],
        }
    }

    #[test]
    fn detects_m3u8_urls_case_insensitively_with_query() {
        assert!(super::super::execute::is_hls_playlist_uri(
            "https://media.example.test/live.M3U8?token=abc"
        ));
        assert!(!super::super::execute::is_hls_playlist_uri(
            "https://media.example.test/video.mp4"
        ));
        assert!(!super::super::execute::is_hls_playlist_uri("not a url"));
    }

    #[test]
    fn maps_hls_output_extensions() {
        let (path, format) = hls_output_path(Path::new("/tmp/stream.m3u8")).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/stream.mp4"));
        assert_eq!(format, MP4);

        let (_, format) = hls_output_path(Path::new("/tmp/stream.mkv")).unwrap();
        assert_eq!(format.muxer, "matroska");

        let error = hls_output_path(Path::new("/tmp/stream.bin")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Unsupported HLS output extension")
        );
    }

    #[test]
    fn rejects_header_injection() {
        let result = validate_headers(&[("X-Test".into(), "safe".into())]);
        assert!(result.is_ok());

        let result = validate_headers(&[("X-Test".into(), "safe\r\nX-Evil: yes".into())]);
        assert!(matches!(result, Err(Aria2Error::Fatal(_))));
    }

    #[test]
    fn selects_aac_filter_only_for_mpegts_sources() {
        assert!(uses_mpegts_segments(&media_playlist()));
        let mut playlist = media_playlist();
        playlist.segments[0].uri = Url::parse("https://media.example.test/seg1.m4s").unwrap();
        assert!(!uses_mpegts_segments(&playlist));
    }

    #[test]
    fn estimates_bandwidth_only_for_master_playlists() {
        let plan = HlsPlan {
            playlist_url: Url::parse("https://media.example.test/media.m3u8").unwrap(),
            media: media_playlist(),
            selected_bandwidth: None,
        };
        assert_eq!(plan.estimated_total_length(), 0);

        let plan = HlsPlan {
            selected_bandwidth: Some(800_000),
            ..plan
        };
        assert_eq!(plan.estimated_total_length(), 400_000);
    }

    #[test]
    fn playlist_error_is_fatal() {
        let error = playlist_error(PlaylistError::MissingHeader.to_string());
        assert!(matches!(error, Aria2Error::Fatal(_)));
    }

    #[test]
    fn missing_ffmpeg_error_explains_user_supplied_requirement() {
        let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let error = ffmpeg_spawn_error_for("ffmpeg", io_error, false);
        let message = error.to_string();
        assert!(message.contains("user-installed FFmpeg"));
        assert!(message.contains("does not bundle FFmpeg"));
        assert!(message.contains("ARIA2C_FFMPEG"));

        let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let error = ffmpeg_spawn_error_for("C:/tools/ffmpeg.exe", io_error, true);
        assert!(error.to_string().contains("configured FFmpeg program"));
    }
}
