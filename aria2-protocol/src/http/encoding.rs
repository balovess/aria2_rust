use flate2::read::{GzDecoder, DeflateDecoder};
use std::io::Read;
use tracing::debug;

pub struct HttpEncoding;

impl HttpEncoding {
    pub fn decode(body: &[u8], content_encoding: Option<&str>) -> Result<Vec<u8>, String> {
        let encoding = content_encoding.unwrap_or("").to_lowercase();
        match encoding.as_str() {
            "" | "identity" => Ok(body.to_vec()),
            "gzip" | "x-gzip" => Self::decode_gzip(body),
            "deflate" => Self::decode_deflate(body),
            "br" => Err("Brotli压缩暂不支持".to_string()),
            "compress" => Err("compress压缩格式已废弃".to_string()),
            other => Err(format!("不支持的Content-Encoding: {}", other)),
        }
    }

    fn decode_gzip(data: &[u8]) -> Result<Vec<u8>, String> {
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let mut decoder = GzDecoder::new(data);
        let mut decompressed = Vec::with_capacity(data.len() * 4);
        decoder.read_to_end(&mut decompressed)
            .map_err(|e| format!("gzip解压失败: {}", e))?;
        debug!("gzip解压完成: {} -> {} 字节", data.len(), decompressed.len());
        Ok(decompressed)
    }

    fn decode_deflate(data: &[u8]) -> Result<Vec<u8>, String> {
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let mut decoder = DeflateDecoder::new(data);
        let mut decompressed = Vec::with_capacity(data.len() * 4);
        decoder.read_to_end(&mut decompressed)
            .map_err(|e| format!("deflate解压失败: {}", e))?;
        debug!("deflate解压完成: {} -> {} 字节", data.len(), decompressed.len());
        Ok(decompressed)
    }

    pub fn detect_best_accept_encoding() -> &'static str {
        "gzip, deflate"
    }

    pub fn is_compressed(content_encoding: Option<&str>) -> bool {
        match content_encoding.map(|e| e.to_lowercase()).as_deref() {
            Some("gzip" | "deflate" | "br" | "compress") => true,
            _ => false,
        }
    }
}

pub struct ChunkedDecoder;

impl ChunkedDecoder {
    pub fn decode(data: &[u8]) -> Result<Vec<u8>, String> {
        let mut result = Vec::new();
        let mut pos = 0;

        while pos < data.len() {
            let line_end = data[pos..].iter()
                .position(|&b| b == b'\r' || b == b'\n')
                .ok_or("分块编码格式错误: 找不到块大小行")?;

            let size_str = unsafe { std::str::from_utf8_unchecked(&data[pos..pos + line_end]) };
            let size_str = size_str.trim();
            let chunk_size: usize = usize::from_str_radix(size_str, 16)
                .map_err(|e| format!("块大小解析失败: {}", e))?;

            pos += line_end;
            if pos < data.len() && data[pos] == b'\r' {
                pos += 1;
            }
            if pos < data.len() && data[pos] == b'\n' {
                pos += 1;
            }

            if chunk_size == 0 {
                break;
            }

            if pos + chunk_size > data.len() {
                return Err("分块数据截断".to_string());
            }

            result.extend_from_slice(&data[pos..pos + chunk_size]);
            pos += chunk_size;

            if pos + 1 < data.len() && data[pos] == b'\r' && data[pos + 1] == b'\n' {
                pos += 2;
            } else if pos < data.len() && data[pos] == b'\n' {
                pos += 1;
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_identity() {
        let data = b"hello world";
        let result = HttpEncoding::decode(data, None).unwrap();
        assert_eq!(result, b"hello world");

        let result = HttpEncoding::decode(data, Some("identity")).unwrap();
        assert_eq!(result, b"hello world");
    }

    #[test]
    fn test_gzip_roundtrip() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let original = b"Hello, this is a test string that should compress well!";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let decompressed = HttpEncoding::decode(&compressed, Some("gzip")).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_detect_compression() {
        assert!(HttpEncoding::is_compressed(Some("gzip")));
        assert!(HttpEncoding::is_compressed(Some("deflate")));
        assert!(HttpEncoding::is_compressed(Some("br")));
        assert!(!HttpEncoding::is_compressed(None));
        assert!(!HttpEncoding::is_compressed(Some("identity")));
    }

    #[test]
    fn test_chunked_decoder() {
        let raw_data = "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let decoded = ChunkedDecoder::decode(raw_data.as_bytes()).unwrap();
        assert_eq!(decoded, b"hello world");
    }
}
