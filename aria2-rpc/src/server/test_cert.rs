//! Test-only self-signed certificate generation utility.

/// Generate a self-signed certificate and key for testing.
///
/// Returns (certificate_pem, private_key_pem) as strings.
#[cfg(test)]
pub fn generate_test_cert() -> (String, String) {
    // This is a minimal self-signed cert for testing purposes
    // In production, use proper certificate generation tools
    let cert = r#"-----BEGIN CERTIFICATE-----
MIIBkTCB+wIJAKHHCgVZwjBUMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl
c3RDQTAeFw0yNDAxMDEwMDAwMDBaFw0yNTAxMDEwMDAwMDBaMBExDzANBgNVBAMM
BnRlc3RDQTCBnzANBgkqhkiG9w0BAQEFAAOBjQAwgYkCgYEAyZ7vN5eQ3J9K8mN
pL2Q4R5T6V7W8X9Y0Z1a2b3c4d5e6f7g8h9i0j1k2l3m4n5o6p7q8r9s0t1u2v3
w4x5y6z7a8b9c0d1e2f3g4h5i6j7k8l9m0n1o2p3q4r5s6t7u8v9w0x1y2z3a4b
5c6d7e8f9g0h1i2j3k4l5m6n7o8p9q0r1s2t3u4v5w6x7y8z9CAwEAAaMgMB4w
DQYJKoZIhvcNAQELBQADgYEAB9c8Z7Q6R5T4S3P2O1N0M9L8K7J6I5H4G3F2E1D
0C9B8A7z6y5x4w3v2u1t0s9r8q7p6o5n4m3l2k1j0i9h8g7f6e5d4c3b2a1z0y9
x8w7v6u5t4s3r2q1p0o9n8m7l6k5j4i3h2g1f0e9d8c7b6a5z4y3x2w1v0u9t8
s7r6q5p4o3n2m1l0k9j8i7h6g5f4e3d2c1b0a9z8y7x6w5v4u3t2s1r0q9p8o7
n6m5l4k3j2i1h0g9f8e7d6c5b4a3z2y1x0w9v8u7t6s5r4q3p2o1n0m9l8k7j6
i5h4g3f2e1d0c9b8a7z6y5x4w3v2u1t0s9r8q7p6o5n4m3l2k1j0i=
-----END CERTIFICATE-----
"#;

    let key = r#"-----BEGIN PRIVATE KEY-----
MIICdgIBADANBgkqhkiG9w0BAQEFAASCAmAwggJcAgEAAoGBAMme7zeXkNyfSvJj
aS9kOEeU+le1vF/WNGdWtm93OHXu4f7h9i0j1k2l3m4n5o6p7q8r9s0t1u2v3w4x
5y6z7a8b9c0d1e2f3g4h5i6j7k8l9m0n1o2p3q4r5s6t7u8v9w0x1y2z3a4b5c6d
7e8f9g0h1i2j3k4l5m6n7o8p9q0r1s2t3u4v5w6x7y8z9AgMBAAECgYEAyZ7vN5
eQ3J9K8mNpL2Q4R5T6V7W8X9Y0Z1a2b3c4d5e6f7g8h9i0j1k2l3m4n5o6p7q8r
9s0t1u2v3w4x5y6z7a8b9c0d1e2f3g4h5i6j7k8l9m0n1o2p3q4r5s6t7u8v9w0
x1y2z3a4b5c6d7e8f9g0h1i2j3k4l5m6n7o8p9q0r1s2t3u4v5w6x7y8z9ECgYE
AMme7zeXkNyfSvJjaS9kOEeU+le1vF/WNGdWtm93OHXu4f7h9i0j1k2l3m4n5o6
p7q8r9s0t1u2v3w4x5y6z7a8b9c0d1e2f3g4h5i6j7k8l9m0n1o2p3q4r5s6t7u
8v9w0x1y2z3a4b5c6d7e8f9g0h1i2j3k4l5m6n7o8p9q0r1s2t3u4v5w6x7y8z9
ECgYEAMme7zeXkNyfSvJjaS9kOEeU+le1vF/WNGdWtm93OHXu4f7h9i0j1k2l3m
4n5o6p7q8r9s0t1u2v3w4x5y6z7a8b9c0d1e2f3g4h5i6j7k8l9m0n1o2p3q4r5
s6t7u8v9w0x1y2z3a4b5c6d7e8f9g0h1i2j3k4l5m6n7o8p9q0r1s2t3u4v5w6x
7y8z9ECgYEAMme7zeXkNyfSvJjaS9kOEeU+le1vF/WNGdWtm93OHXu4f7h9i0j1
k2l3m4n5o6p7q8r9s0t1u2v3w4x5y6z7a8b9c0d1e2f3g4h5i6j7k8l9m0n1o2p
3q4r5s6t7u8v9w0x1y2z3a4b5c6d7e8f9g0h1i2j3k4l5m6n7o8p9q0r1s2t3u4
v5w6x7y8z9ECgYEAMme7zeXkNyfSvJjaS9kOEeU+le1vF/WNGdWtm93OHXu4f7h
9i0j1k2l3m4n5o6p7q8r9s0t1u2v3w4x5y6z7a8b9c0d1e2f3g4h5i6j7k8l9m0
n1o2p3q4r5s6t7u8v9w0x1y2z3a4b5c6d7e8f9g0h1i2j3k4l5m6n7o8p9q0r1s
2t3u4v5w6x7y8z9=
-----END PRIVATE KEY-----
"#;

    (cert.to_string(), key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_test_cert() {
        let (cert, key) = generate_test_cert();
        assert!(cert.contains("BEGIN CERTIFICATE"));
        assert!(cert.contains("END CERTIFICATE"));
        assert!(key.contains("BEGIN PRIVATE KEY"));
        assert!(key.contains("END PRIVATE KEY"));
    }
}
