use super::HttpStreamFilterError;

/// A supported HTTP transfer or content coding selected from response headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpBodyCoding {
    Chunked,
    Gzip,
    Deflate,
    Bzip2,
}

/// Selects body decoders in wire order: transfer coding first, then content
/// codings in the order declared by the response.
pub fn select_http_body_codings(
    content_encoding: Option<&str>,
    transfer_encoding: Option<&str>,
) -> Result<Vec<HttpBodyCoding>, HttpStreamFilterError> {
    let mut codings = Vec::new();

    if let Some(transfer_encoding) = transfer_encoding {
        if !transfer_encoding.trim().eq_ignore_ascii_case("chunked") {
            return Err(HttpStreamFilterError::Protocol(format!(
                "Transfer-Encoding not supported: {transfer_encoding}"
            )));
        }
        codings.push(HttpBodyCoding::Chunked);
    }

    if let Some(content_encoding) = content_encoding {
        for value in content_encoding.split(',') {
            let encoding = value.trim().to_ascii_lowercase();
            match encoding.as_str() {
                "gzip" | "x-gzip" => codings.push(HttpBodyCoding::Gzip),
                "deflate" => codings.push(HttpBodyCoding::Deflate),
                "bzip2" | "x-bzip2" => codings.push(HttpBodyCoding::Bzip2),
                "identity" | "" | "none" => {}
                _ if encoding.contains("lzma") => {
                    tracing::warn!(
                        "LZMA content encoding not yet supported, returning passthrough"
                    );
                }
                "br" => {
                    tracing::debug!("Brotli content encoding detected but not yet implemented");
                }
                _ => tracing::debug!("Unknown content encoding: {}", encoding),
            }
        }
    }

    Ok(codings)
}

/// Detects common compressed body formats from their leading bytes.
pub fn detect_http_body_encoding(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x1f, 0x8b]) {
        "gzip"
    } else if data.starts_with(b"BZ") {
        "bzip2"
    } else if data.first() == Some(&0x78) {
        "deflate"
    } else {
        "identity"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_transfer_before_content_codings() {
        assert_eq!(
            select_http_body_codings(Some("gzip, deflate"), Some(" Chunked ")).unwrap(),
            vec![
                HttpBodyCoding::Chunked,
                HttpBodyCoding::Gzip,
                HttpBodyCoding::Deflate,
            ]
        );
    }

    #[test]
    fn ignores_identity_and_unknown_content_codings() {
        assert_eq!(
            select_http_body_codings(Some("identity, br, custom"), None).unwrap(),
            Vec::<HttpBodyCoding>::new()
        );
    }

    #[test]
    fn rejects_unsupported_transfer_coding() {
        assert!(matches!(
            select_http_body_codings(None, Some("gzip")),
            Err(HttpStreamFilterError::Protocol(_))
        ));
    }

    #[test]
    fn detects_common_compression_signatures() {
        assert_eq!(detect_http_body_encoding(&[0x1f, 0x8b]), "gzip");
        assert_eq!(detect_http_body_encoding(b"BZh"), "bzip2");
        assert_eq!(detect_http_body_encoding(&[0x78, 0x9c]), "deflate");
        assert_eq!(detect_http_body_encoding(b"plain"), "identity");
    }
}
