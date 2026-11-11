//! SFTP URI syntax parsing.

/// Parsed SFTP URI fields before engine credential resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpUri {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub remote_path: String,
}

/// URI syntax errors independent of the download engine's error model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SftpUriError {
    UnsupportedScheme,
    Invalid(&'static str),
}

/// Decode valid percent escapes without treating `+` as a space.
pub fn percent_decode(input: &str) -> String {
    let input = input.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%'
            && let (Some(high), Some(low)) = (
                input.get(index + 1).copied().and_then(hex_value),
                input.get(index + 2).copied().and_then(hex_value),
            )
        {
            bytes.push((high << 4) | low);
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

/// Parse an `sftp://` URI, excluding query and fragment from the remote path.
pub fn parse_uri(uri: &str) -> Result<SftpUri, SftpUriError> {
    let without_scheme = uri
        .strip_prefix("sftp://")
        .ok_or(SftpUriError::UnsupportedScheme)?;
    let authority_end = without_scheme
        .find(['/', '?', '#'])
        .unwrap_or(without_scheme.len());
    let authority = &without_scheme[..authority_end];
    if authority.is_empty() {
        return Err(SftpUriError::Invalid("SFTP URI has no host"));
    }
    let path_and_suffix = &without_scheme[authority_end..];
    let path_end = path_and_suffix
        .find(['?', '#'])
        .unwrap_or(path_and_suffix.len());
    let path = if path_and_suffix.starts_with('/') {
        &path_and_suffix[..path_end]
    } else {
        "/"
    };

    let (userinfo, host_port) = match authority.rfind('@') {
        Some(index) => (&authority[..index], &authority[index + 1..]),
        None => ("", authority),
    };
    if host_port.is_empty() {
        return Err(SftpUriError::Invalid("SFTP URI has no host"));
    }

    let (username, password) = if userinfo.is_empty() {
        (None, None)
    } else if let Some((username, password)) = userinfo.split_once(':') {
        (
            Some(percent_decode(username)),
            Some(percent_decode(password)),
        )
    } else {
        (Some(percent_decode(userinfo)), None)
    };
    if username.as_deref().is_some_and(str::is_empty) {
        return Err(SftpUriError::Invalid("SFTP URI has an empty username"));
    }

    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, suffix) = bracketed
            .split_once(']')
            .ok_or(SftpUriError::Invalid("Invalid bracketed SFTP host"))?;
        if host.is_empty() {
            return Err(SftpUriError::Invalid("SFTP URI has an empty IPv6 host"));
        }
        let port = match suffix {
            "" => super::DEFAULT_PORT,
            value if value.starts_with(':') => parse_port(&value[1..])?,
            _ => return Err(SftpUriError::Invalid("Invalid SFTP host suffix")),
        };
        (host.to_owned(), port)
    } else {
        match host_port.split_once(':') {
            Some((host, port)) => {
                if host.is_empty() || port.contains(':') {
                    return Err(SftpUriError::Invalid("Invalid SFTP host or port"));
                }
                (host.to_owned(), parse_port(port)?)
            }
            None => (host_port.to_owned(), super::DEFAULT_PORT),
        }
    };

    Ok(SftpUri {
        host,
        port,
        username,
        password,
        remote_path: percent_decode(path),
    })
}

fn parse_port(value: &str) -> Result<u16, SftpUriError> {
    if value.is_empty() {
        return Ok(super::DEFAULT_PORT);
    }
    let port = value
        .parse::<u16>()
        .map_err(|_| SftpUriError::Invalid("Invalid SFTP port"))?;
    Ok(if port == 0 { super::DEFAULT_PORT } else { port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_userinfo_ipv6_and_ignores_query_and_fragment() {
        assert_eq!(
            parse_uri("sftp://user%40name:pass%3Aword@[::1]:2222/a%20b?x=1#f"),
            Ok(SftpUri {
                host: "::1".into(),
                port: 2222,
                username: Some("user@name".into()),
                password: Some("pass:word".into()),
                remote_path: "/a b".into(),
            })
        );
    }

    #[test]
    fn rejects_invalid_explicit_hosts_and_ports() {
        assert!(parse_uri("sftp://host:not-a-port/file").is_err());
        assert!(parse_uri("sftp://[::1]suffix/file").is_err());
        assert!(parse_uri("https://host/file").is_err());
    }

    #[test]
    fn percent_decode_preserves_plus_and_invalid_sequences() {
        assert_eq!(percent_decode("a+b%20c%ZZ%3"), "a+b c%ZZ%3");
        assert_eq!(percent_decode("%E6%96%87%E4%BB%B6"), "文件");
    }
}
