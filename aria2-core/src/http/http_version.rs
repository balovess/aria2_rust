/// HTTP protocol preference for HTTP(S) requests.
///
/// For HTTPS, `Auto` advertises HTTP/2 and HTTP/1.1 through TLS ALPN and lets
/// the server select the protocol. It falls back to HTTP/1.1 when HTTP/2 is
/// not negotiated. Plain HTTP uses the client's default HTTP/1 behavior; the
/// explicit variants are strict selections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum HttpVersion {
    /// Prefer HTTP/2 and negotiate HTTP/1.1 when HTTP/2 is unavailable.
    #[default]
    Auto,
    /// Force HTTP/1.1.
    Http11,
    /// Force HTTP/2.
    Http2,
}

impl HttpVersion {
    /// Parse the public `--http-version` option values.
    pub fn parse_option(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "1.1" => Some(Self::Http11),
            "2" => Some(Self::Http2),
            _ => None,
        }
    }

    /// Return the canonical public option value.
    pub const fn option_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Http11 => "1.1",
            Self::Http2 => "2",
        }
    }

    pub(crate) fn configure(self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        match self {
            Self::Auto => builder,
            Self::Http11 => builder.http1_only(),
            Self::Http2 => builder.http2_prior_knowledge(),
        }
    }
}
