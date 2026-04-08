use sha2::{Sha256, Digest};

pub fn build_metalink_v3(
    filename: &str,
    file_size: u64,
    urls: &[(String, i32)],
    sha256_hash: &str,
) -> Vec<u8> {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\">\n");
    xml.push_str("  <files>\n");
    xml.push_str(&format!("    <file name=\"{}\">\n", filename));
    xml.push_str(&format!("      <size>{}</size>\n", file_size));
    if !sha256_hash.is_empty() {
        xml.push_str(&format!("      <hash type=\"sha-256\">{}</hash>\n", sha256_hash));
    }
    for (url, priority) in urls {
        xml.push_str(&format!("      <url priority=\"{}\">{}</url>\n", priority, url));
    }
    xml.push_str("    </file>\n");
    xml.push_str("  </files>\n");
    xml.push_str("</metalink>\n");
    xml.into_bytes()
}

pub fn build_metalink_v4(
    filename: &str,
    file_size: u64,
    urls: &[(String, i32)],
    sha256_hash: &str,
) -> Vec<u8> {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\">\n");
    xml.push_str("  <generator>aria2-rust-test</generator>\n");
    xml.push_str(&format!("  <file name=\"{}\">\n", filename));
    xml.push_str(&format!("    <size>{}</size>\n", file_size));
    if !sha256_hash.is_empty() {
        xml.push_str(&format!("    <hash type=\"sha-256\">{}</hash>\n", sha256_hash));
    }
    for (url, priority) in urls {
        xml.push_str(&format!("    <url priority=\"{}\">{}</url>\n", priority, url));
    }
    xml.push_str("  </file>\n");
    xml.push_str("</metalink>\n");
    xml.into_bytes()
}

pub fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    format!("{:x}", result)
}

pub const SMALL_CONTENT: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
pub const MEDIUM_PATTERN: u8 = 0xAB;

pub fn small_file_sha256() -> &'static str {
    "b06e25eef35b588fae58acfea262783f0c90e50f0bbba32863da044207a6c8bd"
}

pub fn medium_sha256() -> String {
    let data = vec![MEDIUM_PATTERN; 1024 * 1024];
    compute_sha256(&data)
}
