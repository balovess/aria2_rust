//! FTP request syntax for HTTP forward proxies.

use url::Url;

/// Inputs used to serialize an FTP-over-HTTP-proxy GET request.
pub struct ProxyGetRequestOptions<'a> {
    pub ftp_username: &'a str,
    pub proxy_username: &'a str,
    pub proxy_password: &'a str,
    pub user_agent: &'a str,
    pub range_start: u64,
    pub no_cache: bool,
}

/// Serialized HTTP request carrying an FTP URL as its absolute request target.
#[derive(Debug, Clone)]
pub struct ProxyGetRequest {
    pub request_bytes: Vec<u8>,
    pub request_url: String,
}

/// Build the GET request used by an HTTP forward proxy for an FTP resource.
pub fn build_get_request(ftp_url: &Url, options: &ProxyGetRequestOptions<'_>) -> ProxyGetRequest {
    let request_url = build_request_url(ftp_url, options.ftp_username);
    let host_header = build_host_header(ftp_url);
    let mut headers = vec![
        ("Host".to_owned(), host_header),
        ("User-Agent".to_owned(), options.user_agent.to_owned()),
        ("Accept".to_owned(), "*/*".to_owned()),
    ];

    if options.no_cache {
        headers.push(("Pragma".to_owned(), "no-cache".to_owned()));
        headers.push(("Cache-Control".to_owned(), "no-cache".to_owned()));
    }
    headers.push(("Connection".to_owned(), "Keep-Alive".to_owned()));

    if options.range_start > 0 {
        headers.push((
            "Range".to_owned(),
            format!("bytes={}-", options.range_start),
        ));
    }
    if !options.proxy_username.is_empty() {
        headers.push((
            "Proxy-Authorization".to_owned(),
            crate::http::auth::basic_authorization(options.proxy_username, options.proxy_password),
        ));
    }

    let request_bytes = crate::http::request::HttpRequest::get(request_url.clone())
        .with_headers(headers)
        .to_bytes();

    ProxyGetRequest {
        request_bytes,
        request_url,
    }
}

fn build_request_url(ftp_url: &Url, ftp_username: &str) -> String {
    if ftp_url.username().is_empty() && !ftp_username.is_empty() {
        let encoded_user = super::uri::percent_encode_userinfo(ftp_username);
        let url = ftp_url.as_str();
        if let Some(scheme_end) = url.find("://").map(|position| position + 3) {
            let mut request_url = String::with_capacity(url.len() + encoded_user.len() + 1);
            request_url.push_str(&url[..scheme_end]);
            request_url.push_str(&encoded_user);
            request_url.push('@');
            request_url.push_str(&url[scheme_end..]);
            return request_url;
        }
    }

    ftp_url.to_string()
}

fn build_host_header(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(21) | None => host.to_owned(),
        Some(port) => format!("{host}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(ftp_url: &str, options: ProxyGetRequestOptions<'_>) -> ProxyGetRequest {
        build_get_request(&Url::parse(ftp_url).unwrap(), &options)
    }

    #[test]
    fn serializes_default_and_non_default_ports() {
        let options = ProxyGetRequestOptions {
            ftp_username: "",
            proxy_username: "",
            proxy_password: "",
            user_agent: "test-client",
            range_start: 0,
            no_cache: false,
        };
        let default_port = build("ftp://ftp.example.com/file", options);
        let text = String::from_utf8(default_port.request_bytes).unwrap();
        assert!(text.starts_with("GET ftp://ftp.example.com/file HTTP/1.1\r\n"));
        assert!(text.contains("Host: ftp.example.com\r\n"));

        let non_default = build(
            "ftp://ftp.example.com:2121/file",
            ProxyGetRequestOptions {
                ftp_username: "",
                proxy_username: "",
                proxy_password: "",
                user_agent: "test-client",
                range_start: 0,
                no_cache: false,
            },
        );
        assert!(
            String::from_utf8(non_default.request_bytes)
                .unwrap()
                .contains("Host: ftp.example.com:2121\r\n")
        );
    }

    #[test]
    fn serializes_ftp_credentials_proxy_auth_range_and_cache_policy() {
        let request = build(
            "ftp://ftp.example.com/file",
            ProxyGetRequestOptions {
                ftp_username: "user@name",
                proxy_username: "proxy",
                proxy_password: "secret",
                user_agent: "test-client",
                range_start: 4096,
                no_cache: true,
            },
        );
        let text = String::from_utf8(request.request_bytes).unwrap();
        assert!(text.starts_with("GET ftp://user%40name@ftp.example.com/file HTTP/1.1\r\n"));
        assert!(text.contains("Proxy-Authorization: Basic cHJveHk6c2VjcmV0\r\n"));
        assert!(text.contains("Range: bytes=4096-\r\n"));
        assert!(text.contains("Pragma: no-cache\r\n"));
        assert!(text.contains("Cache-Control: no-cache\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn preserves_embedded_ftp_username() {
        let request = build(
            "ftp://existing@ftp.example.com/file",
            ProxyGetRequestOptions {
                ftp_username: "ignored",
                proxy_username: "",
                proxy_password: "",
                user_agent: "test-client",
                range_start: 0,
                no_cache: false,
            },
        );
        assert_eq!(request.request_url, "ftp://existing@ftp.example.com/file");
    }
}
