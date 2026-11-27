//! Typed parsing for HTTP Live Streaming playlists.
//!
//! This module separates playlist syntax from HTTP retrieval, track selection,
//! segment transfer, decryption, and media muxing, which belong to the engine.

use url::Url;

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Media(MediaPlaylist),
    Master(MasterPlaylist),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaPlaylist {
    pub target_duration: Option<u64>,
    pub media_sequence: u64,
    pub playlist_type: Option<MediaPlaylistType>,
    pub end_list: bool,
    pub segments: Vec<MediaSegment>,
}

impl MediaPlaylist {
    /// Sum of the declared segment durations, in seconds.
    pub fn duration_seconds(&self) -> f64 {
        self.segments
            .iter()
            .map(|segment| segment.duration_seconds)
            .sum()
    }

    /// A playlist without `EXT-X-ENDLIST` is treated as a live playlist.
    pub fn is_live(&self) -> bool {
        !self.end_list
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPlaylistType {
    Event,
    Vod,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaSegment {
    pub uri: Url,
    pub duration_seconds: f64,
    pub title: String,
    pub byte_range: Option<ByteRange>,
    pub initialization: Option<InitializationSection>,
    pub key: Option<EncryptionKey>,
    pub discontinuity: bool,
    pub gap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub length: u64,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializationSection {
    pub uri: Url,
    pub byte_range: Option<ByteRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionKey {
    pub method: EncryptionMethod,
    pub uri: Option<Url>,
    pub iv: Option<String>,
    pub key_format: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMethod {
    None,
    Aes128,
    SampleAes,
    SampleAesCtr,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterPlaylist {
    pub variants: Vec<VariantStream>,
    pub renditions: Vec<Rendition>,
}

impl MasterPlaylist {
    /// Select the variant used when the caller exposes no quality preference.
    /// The tie-break order is deterministic.
    pub fn best_variant(&self) -> Option<&VariantStream> {
        self.variants.iter().max_by_key(|variant| {
            (
                variant.average_bandwidth.unwrap_or(variant.bandwidth),
                variant.bandwidth,
                variant
                    .resolution
                    .map(|(width, height)| u64::from(width) * u64::from(height))
                    .unwrap_or(0),
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantStream {
    pub uri: Url,
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    pub codecs: Vec<String>,
    pub resolution: Option<(u32, u32)>,
    pub audio_group: Option<String>,
    pub subtitles_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    pub kind: String,
    pub group_id: String,
    pub name: String,
    pub uri: Option<Url>,
    pub language: Option<String>,
    pub is_default: bool,
    pub autoselect: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlaylistError {
    #[error("playlist is missing the #EXTM3U header")]
    MissingHeader,
    #[error("playlist contains malformed or incomplete {0} data")]
    Malformed(&'static str),
    #[error("playlist URI is invalid: {0}")]
    InvalidUri(String),
    #[error("playlist has no media segments or variant streams")]
    Empty,
}

/// Parse an HLS media or master playlist and resolve its relative URIs.
pub fn parse_playlist(body: &str, playlist_url: &Url) -> Result<Playlist, PlaylistError> {
    let normalized = body.strip_prefix('\u{feff}').unwrap_or(body);
    let mut lines = normalized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err(PlaylistError::MissingHeader);
    }

    let lines: Vec<&str> = lines.collect();
    if lines
        .iter()
        .any(|line| line.starts_with("#EXT-X-STREAM-INF:"))
    {
        parse_master_playlist(&lines, playlist_url).map(Playlist::Master)
    } else {
        parse_media_playlist_lines(&lines, playlist_url).map(Playlist::Media)
    }
}

/// Parse a finite or live HLS media playlist and resolve segment URLs.
pub fn parse_media_playlist(
    body: &str,
    playlist_url: &Url,
) -> Result<MediaPlaylist, PlaylistError> {
    match parse_playlist(body, playlist_url)? {
        Playlist::Media(playlist) => Ok(playlist),
        Playlist::Master(_) => Err(PlaylistError::Malformed("media playlist")),
    }
}

fn parse_master_playlist(
    lines: &[&str],
    playlist_url: &Url,
) -> Result<MasterPlaylist, PlaylistError> {
    let mut variants = Vec::new();
    let mut renditions = Vec::new();
    let mut pending_variant: Option<Vec<(String, String)>> = None;
    for line in lines {
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            if pending_variant.is_some() {
                return Err(PlaylistError::Malformed("EXT-X-STREAM-INF"));
            }
            pending_variant = Some(parse_attributes(attributes)?);
            continue;
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = parse_attributes(attributes)?;
            let kind = required_attr(&attrs, "TYPE")?;
            let group_id = required_attr(&attrs, "GROUP-ID")?;
            let name = required_attr(&attrs, "NAME")?;
            let uri = optional_attr(&attrs, "URI")
                .map(|value| resolve_uri(playlist_url, value))
                .transpose()?;
            renditions.push(Rendition {
                kind: kind.to_owned(),
                group_id: group_id.to_owned(),
                name: name.to_owned(),
                uri,
                language: optional_attr(&attrs, "LANGUAGE").map(str::to_owned),
                is_default: optional_attr(&attrs, "DEFAULT") == Some("YES"),
                autoselect: optional_attr(&attrs, "AUTOSELECT") == Some("YES"),
            });
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let Some(attrs) = pending_variant.take() else {
            return Err(PlaylistError::Malformed("master playlist URI"));
        };
        let bandwidth = required_attr(&attrs, "BANDWIDTH")?
            .parse()
            .map_err(|_| PlaylistError::Malformed("BANDWIDTH"))?;
        if bandwidth == 0 {
            return Err(PlaylistError::Malformed("BANDWIDTH"));
        }
        let average_bandwidth = optional_attr(&attrs, "AVERAGE-BANDWIDTH")
            .map(str::parse)
            .transpose()
            .map_err(|_| PlaylistError::Malformed("AVERAGE-BANDWIDTH"))?;
        let resolution = optional_attr(&attrs, "RESOLUTION")
            .map(parse_resolution)
            .transpose()?;
        variants.push(VariantStream {
            uri: resolve_uri(playlist_url, line)?,
            bandwidth,
            average_bandwidth,
            codecs: optional_attr(&attrs, "CODECS")
                .map(|codecs| codecs.split(',').map(str::to_owned).collect())
                .unwrap_or_default(),
            resolution,
            audio_group: optional_attr(&attrs, "AUDIO").map(str::to_owned),
            subtitles_group: optional_attr(&attrs, "SUBTITLES").map(str::to_owned),
        });
    }
    if pending_variant.is_some() {
        return Err(PlaylistError::Malformed("EXT-X-STREAM-INF URI"));
    }
    if variants.is_empty() {
        return Err(PlaylistError::Empty);
    }
    Ok(MasterPlaylist {
        variants,
        renditions,
    })
}

fn parse_media_playlist_lines(
    lines: &[&str],
    playlist_url: &Url,
) -> Result<MediaPlaylist, PlaylistError> {
    let mut target_duration = None;
    let mut media_sequence = 0;
    let mut playlist_type = None;
    let mut end_list = false;
    let mut pending_duration = None;
    let mut pending_range = None;
    let mut initialization = None;
    let mut key = None;
    let mut discontinuity = false;
    let mut gap = false;
    let mut segments = Vec::new();

    for line in lines {
        if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            let parsed = value
                .parse::<u64>()
                .map_err(|_| PlaylistError::Malformed("TARGETDURATION"))?;
            if parsed == 0 {
                return Err(PlaylistError::Malformed("TARGETDURATION"));
            }
            target_duration = Some(parsed);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value
                .parse()
                .map_err(|_| PlaylistError::Malformed("MEDIA-SEQUENCE"))?;
        } else if let Some(value) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            playlist_type = Some(match value {
                "EVENT" => MediaPlaylistType::Event,
                "VOD" => MediaPlaylistType::Vod,
                _ => return Err(PlaylistError::Malformed("PLAYLIST-TYPE")),
            });
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            if pending_duration.is_some() {
                return Err(PlaylistError::Malformed("EXTINF"));
            }
            let (duration, title) = value.split_once(',').unwrap_or((value, ""));
            let duration_seconds = duration
                .parse::<f64>()
                .ok()
                .filter(|duration| duration.is_finite() && *duration >= 0.0)
                .ok_or(PlaylistError::Malformed("EXTINF duration"))?;
            pending_duration = Some((duration_seconds, title.to_owned()));
        } else if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = Some(parse_byte_range(value)?);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = parse_attributes(value)?;
            let uri = resolve_uri(playlist_url, required_attr(&attrs, "URI")?)?;
            let byte_range = optional_attr(&attrs, "BYTERANGE")
                .map(parse_byte_range)
                .transpose()?;
            initialization = Some(InitializationSection { uri, byte_range });
        } else if let Some(value) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = parse_attributes(value)?;
            let method = match required_attr(&attrs, "METHOD")? {
                "NONE" => EncryptionMethod::None,
                "AES-128" => EncryptionMethod::Aes128,
                "SAMPLE-AES" => EncryptionMethod::SampleAes,
                "SAMPLE-AES-CTR" => EncryptionMethod::SampleAesCtr,
                _ => EncryptionMethod::Unsupported,
            };
            key = if method == EncryptionMethod::None {
                None
            } else {
                Some(EncryptionKey {
                    method,
                    uri: optional_attr(&attrs, "URI")
                        .map(|uri| resolve_uri(playlist_url, uri))
                        .transpose()?,
                    iv: optional_attr(&attrs, "IV").map(str::to_owned),
                    key_format: optional_attr(&attrs, "KEYFORMAT").map(str::to_owned),
                })
            };
        } else if *line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if *line == "#EXT-X-GAP" {
            gap = true;
        } else if *line == "#EXT-X-ENDLIST" {
            end_list = true;
        } else if line.starts_with('#') {
            continue;
        } else {
            let (duration_seconds, title) = pending_duration
                .take()
                .ok_or(PlaylistError::Malformed("segment without EXTINF"))?;
            segments.push(MediaSegment {
                uri: resolve_uri(playlist_url, line)?,
                duration_seconds,
                title,
                byte_range: pending_range.take(),
                initialization: initialization.clone(),
                key: key.clone(),
                discontinuity,
                gap,
            });
            discontinuity = false;
            gap = false;
        }
    }
    if pending_duration.is_some() {
        return Err(PlaylistError::Malformed("EXTINF segment URI"));
    }
    if segments.is_empty() {
        return Err(PlaylistError::Empty);
    }
    if target_duration.is_none() {
        return Err(PlaylistError::Malformed("TARGETDURATION"));
    }
    Ok(MediaPlaylist {
        target_duration,
        media_sequence,
        playlist_type,
        end_list,
        segments,
    })
}

fn parse_attributes(input: &str) -> Result<Vec<(String, String)>, PlaylistError> {
    let bytes = input.as_bytes();
    let mut result = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut pieces = Vec::new();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                pieces.push(&input[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if quoted {
        return Err(PlaylistError::Malformed("attribute list"));
    }
    pieces.push(&input[start..]);
    for piece in pieces {
        let (name, value) = piece
            .split_once('=')
            .ok_or(PlaylistError::Malformed("attribute list"))?;
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        if name.is_empty() || value.is_empty() || result.iter().any(|(key, _)| key == name) {
            return Err(PlaylistError::Malformed("attribute list"));
        }
        result.push((name.to_owned(), value.to_owned()));
    }
    Ok(result)
}

fn required_attr<'a>(attrs: &'a [(String, String)], name: &str) -> Result<&'a str, PlaylistError> {
    optional_attr(attrs, name).ok_or(PlaylistError::Malformed("required attribute"))
}

fn optional_attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn parse_resolution(value: &str) -> Result<(u32, u32), PlaylistError> {
    let (width, height) = value
        .split_once('x')
        .ok_or(PlaylistError::Malformed("RESOLUTION"))?;
    Ok((
        width
            .parse()
            .map_err(|_| PlaylistError::Malformed("RESOLUTION"))?,
        height
            .parse()
            .map_err(|_| PlaylistError::Malformed("RESOLUTION"))?,
    ))
}

fn parse_byte_range(value: &str) -> Result<ByteRange, PlaylistError> {
    let (length, offset) = value.split_once('@').unwrap_or((value, ""));
    let length = length
        .parse()
        .map_err(|_| PlaylistError::Malformed("BYTERANGE"))?;
    if length == 0 {
        return Err(PlaylistError::Malformed("BYTERANGE"));
    }
    let offset = if offset.is_empty() {
        None
    } else {
        Some(
            offset
                .parse()
                .map_err(|_| PlaylistError::Malformed("BYTERANGE"))?,
        )
    };
    Ok(ByteRange { length, offset })
}

fn resolve_uri(base: &Url, reference: &str) -> Result<Url, PlaylistError> {
    base.join(reference)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .ok_or_else(|| PlaylistError::InvalidUri(reference.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://media.example.test/hls/master.m3u8").unwrap()
    }

    #[test]
    fn parses_vod_media_playlist_with_common_tags() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:3\n",
            "#EXT-X-TARGETDURATION:10\n",
            "#EXT-X-MEDIA-SEQUENCE:7\n",
            "#EXT-X-PLAYLIST-TYPE:VOD\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x00112233445566778899aabbccddeeff\n",
            "#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"800@0\"\n",
            "#EXTINF:9.5,first segment\n",
            "#EXT-X-BYTERANGE:1000@800\n",
            "seg1.ts\n",
            "#EXT-X-DISCONTINUITY\n",
            "#EXT-X-GAP\n",
            "#EXTINF:10.0,second segment\n",
            "seg2.ts\n",
            "#EXT-X-ENDLIST\n",
        );
        let playlist = parse_media_playlist(body, &base()).unwrap();

        assert_eq!(playlist.media_sequence, 7);
        assert_eq!(playlist.playlist_type, Some(MediaPlaylistType::Vod));
        assert!(!playlist.is_live());
        assert_eq!(playlist.duration_seconds(), 19.5);
        assert_eq!(playlist.segments.len(), 2);
        assert_eq!(
            playlist.segments[0].uri.as_str(),
            "https://media.example.test/hls/seg1.ts"
        );
        assert_eq!(playlist.segments[0].title, "first segment");
        assert_eq!(
            playlist.segments[0].byte_range,
            Some(ByteRange {
                length: 1000,
                offset: Some(800)
            })
        );
        assert_eq!(
            playlist.segments[0].key.as_ref().unwrap().method,
            EncryptionMethod::Aes128
        );
        assert_eq!(
            playlist.segments[0]
                .initialization
                .as_ref()
                .unwrap()
                .uri
                .as_str(),
            "https://media.example.test/hls/init.mp4"
        );
        assert!(playlist.segments[1].discontinuity);
        assert!(playlist.segments[1].gap);
    }

    #[test]
    fn parses_master_playlist_and_selects_best_variant() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",",
            "DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\",LANGUAGE=\"en\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=800000,AVERAGE-BANDWIDTH=700000,",
            "CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720,AUDIO=\"audio\"\n",
            "video-high.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=300000,CODECS=\"avc1.42e01e,mp4a.40.2\",",
            "RESOLUTION=640x360\n",
            "video-low.m3u8\n",
        );
        let Playlist::Master(playlist) = parse_playlist(body, &base()).unwrap() else {
            panic!("expected a master playlist");
        };

        assert_eq!(playlist.variants.len(), 2);
        assert_eq!(playlist.renditions.len(), 1);
        assert_eq!(playlist.renditions[0].kind, "AUDIO");
        assert_eq!(playlist.variants[0].codecs.len(), 2);
        assert_eq!(playlist.variants[0].resolution, Some((1280, 720)));
        assert_eq!(
            playlist.best_variant().unwrap().uri.as_str(),
            "https://media.example.test/hls/video-high.m3u8"
        );
    }

    #[test]
    fn treats_playlist_without_end_list_as_live() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-TARGETDURATION:6\n",
            "#EXTINF:6.0,\n",
            "live-1.ts\n",
        );
        let playlist = parse_media_playlist(body, &base()).unwrap();
        assert!(playlist.is_live());
        assert_eq!(playlist.duration_seconds(), 6.0);
    }

    #[test]
    fn rejects_invalid_playlists() {
        assert_eq!(
            parse_media_playlist("not a playlist", &base()).unwrap_err(),
            PlaylistError::MissingHeader
        );
        assert!(matches!(
            parse_media_playlist("#EXTM3U\n#EXT-X-TARGETDURATION:0\n", &base()).unwrap_err(),
            PlaylistError::Malformed("TARGETDURATION")
        ));
        assert!(matches!(
            parse_playlist(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=0\nlow.m3u8\n",
                &base()
            )
            .unwrap_err(),
            PlaylistError::Malformed("BANDWIDTH")
        ));
        assert!(matches!(
            parse_media_playlist("#EXTM3U\n#EXT-X-TARGETDURATION:4\nsegment.ts\n", &base())
                .unwrap_err(),
            PlaylistError::Malformed("segment without EXTINF")
        ));
    }
}
