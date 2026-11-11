//! HTTP authentication challenge syntax and protocol values.

use std::collections::HashMap;

/// A parsed HTTP Digest authentication challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub qop: Option<String>,
    pub algorithm: String,
    pub opaque: Option<String>,
    pub stale: bool,
}

/// Parsed Digest challenge parameters before applying RFC validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestChallengeParameters {
    pub realm: Option<String>,
    pub nonce: Option<String>,
    pub qop: Option<String>,
    pub algorithm: Option<String>,
    pub opaque: Option<String>,
    pub stale: bool,
}

/// Parse failure for a Digest challenge header.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DigestChallengeError {
    #[error("Not a Digest challenge: missing 'Digest' scheme")]
    WrongScheme,
    #[error("Invalid Digest challenge parameter")]
    InvalidParameter,
    #[error("Unterminated quoted Digest challenge parameter")]
    UnterminatedQuotedParameter,
    #[error("Invalid Digest challenge parameter separator")]
    InvalidSeparator,
    #[error("Missing required 'nonce' parameter in Digest challenge")]
    MissingNonce,
    #[error("Unsupported Digest qop: {0}")]
    UnsupportedQop(String),
    #[error("Unsupported Digest algorithm: {0}")]
    UnsupportedAlgorithm(String),
}

impl DigestChallenge {
    /// Parse and validate a `WWW-Authenticate` Digest challenge.
    pub fn parse(header_value: &str) -> Result<Self, DigestChallengeError> {
        let parameters = parse_challenge_parameters(header_value)?;
        let nonce = parameters.nonce.ok_or(DigestChallengeError::MissingNonce)?;
        Ok(Self {
            realm: parameters.realm.unwrap_or_default(),
            nonce,
            qop: select_qop(parameters.qop.as_deref())?,
            algorithm: normalize_algorithm(parameters.algorithm.as_deref())?,
            opaque: parameters.opaque,
            stale: parameters.stale,
        })
    }
}

/// Parse Digest parameter syntax without requiring a nonce or supported
/// algorithm/qop. This supports compatibility adapters that expose the raw
/// historical challenge model.
pub fn parse_challenge_parameters(
    header_value: &str,
) -> Result<DigestChallengeParameters, DigestChallengeError> {
    let mut header_parts = header_value
        .trim()
        .splitn(2, |character: char| character.is_ascii_whitespace());
    if !header_parts
        .next()
        .unwrap_or_default()
        .eq_ignore_ascii_case("Digest")
    {
        return Err(DigestChallengeError::WrongScheme);
    }
    let parameters = parse_parameters(header_parts.next().unwrap_or_default())?;
    Ok(DigestChallengeParameters {
        realm: parameters.get("realm").cloned(),
        nonce: parameters.get("nonce").cloned(),
        qop: parameters.get("qop").cloned(),
        algorithm: parameters.get("algorithm").cloned(),
        opaque: parameters.get("opaque").cloned(),
        stale: parameters
            .get("stale")
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
    })
}

/// Computed HTTP Digest response, ready for serialization as an
/// `Authorization` header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestResponse {
    pub username: String,
    pub realm: String,
    pub nonce: String,
    pub uri: String,
    pub qop: Option<String>,
    pub nc: u32,
    pub cnonce: String,
    pub response: String,
    pub algorithm: String,
    pub opaque: Option<String>,
}

impl DigestResponse {
    /// Compute RFC 7616 Digest fields from a parsed challenge.
    pub fn compute(
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        challenge: &DigestChallenge,
        nc: u32,
    ) -> Self {
        let cnonce = format!("{:016x}", rand::random::<u64>());
        Self::compute_with_cnonce(username, password, method, uri, challenge, nc, &cnonce)
    }

    /// Compute a response using the caller-provided client nonce.
    ///
    /// This is useful for protocol integrations that own nonce generation or
    /// must preserve an existing deterministic API.
    pub fn compute_with_cnonce(
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        challenge: &DigestChallenge,
        nc: u32,
        cnonce: &str,
    ) -> Self {
        Self::compute_with_body_and_cnonce(
            username, password, method, uri, challenge, nc, cnonce, None,
        )
    }

    /// Compute a response with an optional entity body for `auth-int`.
    pub fn compute_with_body_and_cnonce(
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        challenge: &DigestChallenge,
        nc: u32,
        cnonce: &str,
        entity_body: Option<&[u8]>,
    ) -> Self {
        let algorithm = challenge.algorithm.to_ascii_uppercase();
        let session_algorithm = algorithm.ends_with("-SESS");
        let qop = select_qop(challenge.qop.as_deref()).ok().flatten();

        let initial_ha1 = hash_parts(
            &algorithm,
            &[
                username.as_bytes(),
                b":",
                challenge.realm.as_bytes(),
                b":",
                password.as_bytes(),
            ],
        );
        let ha1 = if session_algorithm {
            hash_parts(
                &algorithm,
                &[
                    initial_ha1.as_bytes(),
                    b":",
                    challenge.nonce.as_bytes(),
                    b":",
                    cnonce.as_bytes(),
                ],
            )
        } else {
            initial_ha1
        };
        let ha2 = if qop.as_deref() == Some("auth-int") {
            let entity_hash = hash_parts(&algorithm, &[entity_body.unwrap_or_default()]);
            hash_parts(
                &algorithm,
                &[
                    method.as_bytes(),
                    b":",
                    uri.as_bytes(),
                    b":",
                    entity_hash.as_bytes(),
                ],
            )
        } else {
            hash_parts(&algorithm, &[method.as_bytes(), b":", uri.as_bytes()])
        };
        let nc_string = format!("{nc:08x}");
        let response = match qop.as_deref() {
            Some(qop) => hash_parts(
                &algorithm,
                &[
                    ha1.as_bytes(),
                    b":",
                    challenge.nonce.as_bytes(),
                    b":",
                    nc_string.as_bytes(),
                    b":",
                    cnonce.as_bytes(),
                    b":",
                    qop.as_bytes(),
                    b":",
                    ha2.as_bytes(),
                ],
            ),
            None => hash_parts(
                &algorithm,
                &[
                    ha1.as_bytes(),
                    b":",
                    challenge.nonce.as_bytes(),
                    b":",
                    ha2.as_bytes(),
                ],
            ),
        };

        Self {
            username: username.to_owned(),
            realm: challenge.realm.clone(),
            nonce: challenge.nonce.clone(),
            uri: uri.to_owned(),
            qop,
            nc,
            cnonce: cnonce.to_owned(),
            response,
            algorithm: challenge.algorithm.clone(),
            opaque: challenge.opaque.clone(),
        }
    }

    /// Serialize the computed values as an HTTP `Authorization` field value.
    pub fn to_header_value(&self) -> String {
        let mut fields = vec![
            format!("username=\"{}\"", escape_quoted(&self.username)),
            format!("realm=\"{}\"", escape_quoted(&self.realm)),
            format!("nonce=\"{}\"", escape_quoted(&self.nonce)),
            format!("uri=\"{}\"", escape_quoted(&self.uri)),
            format!("response=\"{}\"", self.response),
            format!("algorithm={}", self.algorithm),
        ];
        if let Some(qop) = self.qop.as_deref() {
            fields.push(format!("qop={qop}"));
            fields.push(format!("nc={:08x}", self.nc));
            fields.push(format!("cnonce=\"{}\"", escape_quoted(&self.cnonce)));
        } else if self.algorithm.eq_ignore_ascii_case("MD5-sess")
            || self.algorithm.eq_ignore_ascii_case("SHA-256-sess")
            || self.algorithm.eq_ignore_ascii_case("SHA-512-256-sess")
        {
            fields.push(format!("cnonce=\"{}\"", escape_quoted(&self.cnonce)));
        }
        if let Some(opaque) = self.opaque.as_deref() {
            fields.push(format!("opaque=\"{}\"", escape_quoted(opaque)));
        }
        format!("Digest {}", fields.join(", "))
    }
}

/// Hash bytes using one of the RFC 7616 algorithms supported by this module.
pub fn hash_value(algorithm: &str, value: &[u8]) -> Result<String, DigestChallengeError> {
    let algorithm = normalize_algorithm(Some(algorithm))?;
    Ok(hash_parts(&algorithm.to_ascii_uppercase(), &[value]))
}

fn hash_parts(algorithm: &str, parts: &[&[u8]]) -> String {
    use digest::Digest;

    fn update(hasher: &mut impl Digest, parts: &[&[u8]]) {
        for part in parts {
            hasher.update(part);
        }
    }

    match algorithm {
        "SHA-256" | "SHA-256-SESS" => {
            let mut hasher = sha2::Sha256::new();
            update(&mut hasher, parts);
            hex::encode(hasher.finalize())
        }
        "SHA-512-256" | "SHA-512-256-SESS" => {
            let mut hasher = sha2::Sha512_256::new();
            update(&mut hasher, parts);
            hex::encode(hasher.finalize())
        }
        _ => {
            let mut hasher = md5::Md5::new();
            update(&mut hasher, parts);
            hex::encode(hasher.finalize())
        }
    }
}

fn escape_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_parameters(input: &str) -> Result<HashMap<String, String>, DigestChallengeError> {
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut parameters = HashMap::new();

    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b',') {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }

        let key_start = index;
        while index < bytes.len() && bytes[index] != b'=' && bytes[index] != b',' {
            index += 1;
        }
        let key = input[key_start..index].trim();
        if key.is_empty() || index == bytes.len() || bytes[index] != b'=' {
            return Err(DigestChallengeError::InvalidParameter);
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        let value = if bytes.get(index) == Some(&b'"') {
            index += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' if index + 1 < bytes.len() => {
                        index += 1;
                        let escaped = input[index..]
                            .chars()
                            .next()
                            .ok_or(DigestChallengeError::InvalidParameter)?;
                        value.push(escaped);
                        index += escaped.len_utf8();
                    }
                    b'"' => {
                        index += 1;
                        closed = true;
                        break;
                    }
                    byte => {
                        if byte.is_ascii() {
                            value.push(byte as char);
                            index += 1;
                        } else {
                            let character = input[index..]
                                .chars()
                                .next()
                                .ok_or(DigestChallengeError::InvalidParameter)?;
                            value.push(character);
                            index += character.len_utf8();
                        }
                    }
                }
            }
            if !closed {
                return Err(DigestChallengeError::UnterminatedQuotedParameter);
            }
            value
        } else {
            let value_start = index;
            while index < bytes.len() && bytes[index] != b',' {
                index += 1;
            }
            input[value_start..index].trim().to_string()
        };

        parameters.insert(key.to_ascii_lowercase(), value);
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index < bytes.len() && bytes[index] != b',' {
            return Err(DigestChallengeError::InvalidSeparator);
        }
    }
    Ok(parameters)
}

fn select_qop(value: Option<&str>) -> Result<Option<String>, DigestChallengeError> {
    let Some(value) = value else {
        return Ok(None);
    };
    value
        .split(',')
        .map(str::trim)
        .find(|qop| qop.eq_ignore_ascii_case("auth"))
        .or_else(|| {
            value
                .split(',')
                .map(str::trim)
                .find(|qop| qop.eq_ignore_ascii_case("auth-int"))
        })
        .map(|qop| qop.to_ascii_lowercase())
        .map(Some)
        .ok_or_else(|| DigestChallengeError::UnsupportedQop(value.to_string()))
}

fn normalize_algorithm(value: Option<&str>) -> Result<String, DigestChallengeError> {
    let algorithm = value.unwrap_or("MD5");
    let canonical = match algorithm.to_ascii_uppercase().as_str() {
        "MD5" => "MD5",
        "MD5-SESS" => "MD5-sess",
        "SHA-256" => "SHA-256",
        "SHA-256-SESS" => "SHA-256-sess",
        "SHA-512-256" => "SHA-512-256",
        "SHA-512-256-SESS" => "SHA-512-256-sess",
        _ => {
            return Err(DigestChallengeError::UnsupportedAlgorithm(
                algorithm.to_string(),
            ));
        }
    };
    Ok(canonical.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_rfc_2617_digest_response_vector() {
        let challenge = DigestChallenge::parse(
            r#"Digest realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", algorithm="MD5", opaque="5ccc069c403ebaf9f0171e9517bf40c9""#,
        )
        .unwrap();
        let response = DigestResponse::compute_with_cnonce(
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            &challenge,
            1,
            "0a4f113b",
        );

        assert_eq!(response.response, "6629fae49393a05397450978507c4ef1");
        assert_eq!(
            response.to_header_value(),
            concat!(
                "Digest username=\"Mufasa\", realm=\"testrealm@host.com\", ",
                "nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", ",
                "uri=\"/dir/index.html\", response=\"6629fae49393a05397450978507c4ef1\", ",
                "algorithm=MD5, qop=auth, nc=00000001, cnonce=\"0a4f113b\", ",
                "opaque=\"5ccc069c403ebaf9f0171e9517bf40c9\""
            )
        );
    }

    #[test]
    fn digest_response_escapes_quoted_parameters() {
        let response = DigestResponse {
            username: "u\\\"ser".to_owned(),
            realm: "r\\ealm".to_owned(),
            nonce: "nonce".to_owned(),
            uri: "/".to_owned(),
            qop: None,
            nc: 1,
            cnonce: String::new(),
            response: "abcd".to_owned(),
            algorithm: "MD5".to_owned(),
            opaque: None,
        };
        assert!(
            response
                .to_header_value()
                .contains("username=\"u\\\\\\\"ser\"")
        );
        assert!(response.to_header_value().contains("realm=\"r\\\\ealm\""));
    }

    #[test]
    fn parses_quoted_commas_escapes_and_supported_algorithms() {
        let challenge = DigestChallenge::parse(
            r#"dIgEsT realm="download, private", nonce="n\"1", qop="auth-int, auth", algorithm=SHA-256-SESS"#,
        )
        .unwrap();
        assert_eq!(challenge.realm, "download, private");
        assert_eq!(challenge.nonce, "n\"1");
        assert_eq!(challenge.qop.as_deref(), Some("auth"));
        assert_eq!(challenge.algorithm, "SHA-256-sess");
    }

    #[test]
    fn preserves_unicode_in_quoted_challenge_parameters() {
        let challenge =
            DigestChallenge::parse(r#"Digest realm="下载服务", nonce="中文-nonce""#).unwrap();
        assert_eq!(challenge.realm, "下载服务");
        assert_eq!(challenge.nonce, "中文-nonce");
    }

    #[test]
    fn rejects_invalid_scheme_nonce_parameters_qop_and_algorithm() {
        assert_eq!(
            DigestChallenge::parse("Basic realm=private").unwrap_err(),
            DigestChallengeError::WrongScheme
        );
        assert_eq!(
            DigestChallenge::parse("Digest realm=private").unwrap_err(),
            DigestChallengeError::MissingNonce
        );
        assert_eq!(
            DigestChallenge::parse("Digest nonce=abc, broken").unwrap_err(),
            DigestChallengeError::InvalidParameter
        );
        assert!(DigestChallenge::parse("Digest nonce=abc, qop=auth-int-only").is_err());
        assert!(DigestChallenge::parse("Digest nonce=abc, algorithm=SHA-1").is_err());
    }
}
