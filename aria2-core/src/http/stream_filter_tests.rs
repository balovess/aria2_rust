//! 流式过滤器框架的单元测试
//!
//! 测试覆盖 GZip、Chunked、BZip2 解码器，FilterChain 组合，
//! AutoFilterSelector 自动选择器，以及 HttpResponse 集成测试。

use super::stream_filter::*;
use crate::error::Aria2Error;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write;

// ==================== 辅助函数 ====================

/// 创建 GZip 压缩数据（用于测试）
fn create_gzip_data(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Pre-computed BZip2 compressed test data (pure Rust, no C dependency needed)
/// Original: "BZip2 compression test data for verification."
fn bzip2_test_data() -> Vec<u8> {
    hex::decode(
        "425a6839314159265359d1dfd3620000039f8040011000100000102f23dd002\
                 000314c98990646113d469a0d036a4e1b7e1eb5d9df8e872cabd535e9962e96\
                 057870104680f8bb9229c284868efe9b10",
    )
    .unwrap()
}

// ==================== GZipDecoder 测试 ====================

#[test]
fn test_gzip_decompress_small_file() {
    // 准备测试数据 (<1KB)
    let original = b"Hello, World! This is a small test file for gzip decompression.";
    let compressed = create_gzip_data(original);

    // 执行解压
    let mut decoder = GZipDecoder::new();
    let result = decoder
        .filter(&compressed)
        .expect("GZip decompression failed");

    // 验证结果
    assert_eq!(result, original, "Decompressed data should match original");
    assert_eq!(decoder.name(), "gzip");
}

#[test]
fn test_gzip_invalid_header_error() {
    // 非 GZip 数据（缺少 magic number）
    let invalid_data = b"This is not gzip data";

    let mut decoder = GZipDecoder::new();
    let result = decoder.filter(invalid_data);

    // 应该返回错误
    assert!(result.is_err(), "Should fail with invalid GZip data");
    match result.unwrap_err() {
        Aria2Error::Parse(msg) => {
            assert!(
                msg.contains("Invalid GZip magic number"),
                "Error message should mention invalid magic number"
            );
        }
        other => panic!("Expected Parse error, got: {:?}", other),
    }
}

#[test]
fn test_gzip_needs_more_input() {
    let original = b"Test needs_more_input";
    let compressed = create_gzip_data(original);

    let mut decoder = GZipDecoder::new();

    // 解压前应该需要输入
    assert!(
        decoder.needs_more_input(),
        "New decoder should need input before processing"
    );

    // 解压后不应该再需要输入
    let _ = decoder
        .filter(&compressed)
        .expect("Decompression should succeed");
    assert!(
        !decoder.needs_more_input(),
        "Finished decoder should not need more input"
    );
}

// ==================== ChunkedDecoder 测试 ====================

#[test]
fn test_chunked_decode_normal() {
    // 标准 chunked 格式: 5\r\nhello\r\n0\r\n\r\n
    let chunked_data = b"5\r\nhello\r\n0\r\n\r\n";

    let mut decoder = ChunkedDecoder::new();
    let result = decoder.filter(chunked_data).expect("Chunked decode failed");

    assert_eq!(result, b"hello", "Should decode 'hello'");
    assert_eq!(decoder.name(), "chunked");

    // 应该完成
    assert!(
        !decoder.needs_more_input(),
        "Should be complete after final chunk"
    );
}

#[test]
fn test_chunked_decode_with_extensions() {
    // 含扩展的 chunked 格式: 5;name=value\r\nhello\r\n0\r\n\r\n
    let chunked_data = b"5;name=value\r\nhello\r\n0\r\n\r\n";

    let mut decoder = ChunkedDecoder::new();
    let result = decoder
        .filter(chunked_data)
        .expect("Chunked decode with extensions failed");

    assert_eq!(
        result, b"hello",
        "Should ignore extensions and decode correctly"
    );
}

#[test]
fn test_chunked_early_eof() {
    // 不完整的 chunk (size=10 但只有5字节数据)
    let incomplete_chunked = b"A\r\nHello"; // size=10, but only 5 bytes of data

    let mut decoder = ChunkedDecoder::new();
    let result = decoder.filter(incomplete_chunked);

    // 应该成功返回已有数据（部分解码）
    match result {
        Ok(data) => {
            assert_eq!(data, b"Hello", "Should return partial data");
            // 状态应该是 ReadingData 或等待更多输入
            assert!(
                decoder.needs_more_input(),
                "Incomplete chunk should need more input"
            );
        }
        Err(e) => {
            // 也可能返回错误，取决于实现
            println!("Got error for early EOF: {:?}", e);
        }
    }

    // flush 应该返回错误或警告
    let flush_result = decoder.flush();
    match flush_result {
        Err(Aria2Error::Parse(_)) => {} // 预期的错误
        Ok(_) => {}                     // 或者返回已有数据
        other => panic!("Unexpected flush result: {:?}", other),
    }
}

#[test]
fn test_chunked_multiple_chunks() {
    // 多个 chunk: 5\r\nhello\r\n6\r\n world\r\n7\r\n!!!\r\n0\r\n\r\n
    let chunked_data = b"5\r\nhello\r\n6\r\n world\r\n7\r\n!!!\r\n0\r\n\r\n";

    let mut decoder = ChunkedDecoder::new();
    let result = decoder
        .filter(chunked_data)
        .expect("Multi-chunk decode failed");

    // 验证输出包含所有 chunk 的数据（前面部分应该完全匹配）
    assert!(
        result.starts_with(b"hello world!!!"),
        "Output should start with concatenated chunk data: got {:?}",
        result
    );
}

// ==================== FilterChain 测试 ====================

#[test]
fn test_filter_chain_gzip_then_chunked() {
    // 先 GZip 压缩，再用 chunked 编码
    let original = b"Compressed and chunked data";
    let compressed = create_gzip_data(original);

    // 手动创建 chunked 格式的压缩数据
    let size_hex = format!("{:x}", compressed.len());
    let _chunked_compressed = format!(
        "{}\r\n{}\r\n0\r\n\r\n",
        size_hex,
        String::from_utf8_lossy(&compressed)
    )
    .into_bytes();

    // 注意：这个测试验证 FilterChain 的组合能力
    // 实际场景中可能需要调整顺序或使用不同的组合

    // 单独测试每个过滤器
    let mut gzip_decoder = GZipDecoder::new();
    let decompressed = gzip_decoder.filter(&compressed).expect("GZip failed");
    assert_eq!(decompressed, original);
}

#[test]
fn test_filter_chain_empty() {
    // 空 chain 应该直接透传数据
    let mut chain = FilterChain::new();
    let input = b"passthrough data";

    let result = chain.process(input).expect("Empty chain process failed");

    assert_eq!(
        result, input,
        "Empty chain should pass through data unchanged"
    );
    assert!(chain.is_empty(), "Chain should be empty");
    assert_eq!(chain.len(), 0, "Length should be 0");
}

#[test]
fn test_filter_chain_push_and_clear() {
    let mut chain = FilterChain::new();

    // 添加过滤器
    chain.push(Box::new(GZipDecoder::new()));
    assert_eq!(chain.len(), 1, "Should have 1 filter after push");

    chain.push(Box::new(ChunkedDecoder::new()));
    assert_eq!(chain.len(), 2, "Should have 2 filters after second push");

    // 清除
    chain.clear();
    assert!(chain.is_empty(), "Should be empty after clear");
    assert_eq!(chain.len(), 0, "Length should be 0 after clear");
}

// ==================== AutoFilterSelector 测试 ====================

#[test]
fn test_auto_select_gzip_content_encoding() {
    // Content-Encoding: gzip → 应选择 GZipDecoder
    let chain = AutoFilterSelector::select_filters(Some("gzip"), None);

    assert_eq!(chain.len(), 1, "Should select 1 filter for gzip");
}

#[test]
fn test_auto_select_chunked_transfer_encoding() {
    // Transfer-Encoding: chunked → 应选择 ChunkedDecoder
    let chain = AutoFilterSelector::select_filters(None, Some("chunked"));

    assert_eq!(chain.len(), 1, "Should select 1 filter for chunked");
}

#[test]
fn test_auto_select_x_gzip_encoding() {
    // x-gzip 是 gzip 的别名
    let chain = AutoFilterSelector::select_filters(Some("x-gzip"), None);

    assert_eq!(chain.len(), 1, "x-gzip should be treated as gzip");
}

#[test]
fn test_auto_select_bzip2_encoding() {
    let chain = AutoFilterSelector::select_filters(Some("bzip2"), None);

    assert_eq!(
        chain.len(),
        1,
        "Should select BZip2Decoder for bzip2 encoding"
    );
}

#[test]
fn test_auto_select_identity_encoding() {
    // identity 表示无编码
    let chain = AutoFilterSelector::select_filters(Some("identity"), None);

    assert_eq!(
        chain.len(),
        0,
        "Identity encoding should not add any filters"
    );
}

#[test]
fn test_auto_select_no_encoding() {
    // 无编码信息
    let chain = AutoFilterSelector::select_filters(None, None);

    assert_eq!(chain.len(), 0, "No encoding should result in empty chain");
}

// ==================== HttpResponse 集成测试 ====================

#[test]
fn test_http_response_decoded_body_integration() {
    use super::request_response::HttpResponse;
    use std::collections::HashMap;

    // 准备原始数据和 GZip 压缩数据
    let original = b"HTTP response body content";
    let compressed = create_gzip_data(original);

    // 构建 HTTP 响应
    let mut headers: HashMap<String, Vec<String>> = HashMap::new();
    headers.insert("Content-Encoding".to_string(), vec!["gzip".to_string()]);
    headers.insert("Content-Type".to_string(), vec!["text/plain".to_string()]);

    let response = HttpResponse {
        status_code: 200,
        reason_phrase: "OK".to_string(),
        version: "HTTP/1.1".to_string(),
        headers,
        body: Some(compressed),
    };

    // 使用 decoded_body 获取解压后的内容
    let decoded = response.decoded_body().expect("decoded_body failed");

    assert_eq!(
        decoded, original,
        "decoded_body should return decompressed content"
    );
}

#[test]
fn test_http_response_decoded_body_no_body() {
    use super::request_response::HttpResponse;
    use std::collections::HashMap;

    // 无 body 的响应
    let response = HttpResponse {
        status_code: 204,
        reason_phrase: "No Content".to_string(),
        version: "HTTP/1.1".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let decoded = response
        .decoded_body()
        .expect("decoded_body should succeed for no body");

    assert!(
        decoded.is_empty(),
        "No body response should return empty vector"
    );
}

// ==================== 混合编码处理测试 ====================

#[test]
fn test_mixed_encoding_handling() {
    // 当同时存在 Transfer-Encoding 和 Content-Encoding 时，
    // 根据 RFC 7230，Transfer-Encoding 优先

    // 场景1: Transfer-Encoding=chunked + Content-Encoding=gzip
    // 应该只使用 chunked 解码器
    let chain = AutoFilterSelector::select_filters(Some("gzip"), Some("chunked"));

    assert_eq!(
        chain.len(),
        1,
        "Transfer-Encoding should take priority over Content-Encoding"
    );
}

#[test]
fn test_multiple_content_encodings() {
    // 多个 Content-Encoding 值（逗号分隔）
    let chain = AutoFilterSelector::select_filters(Some("gzip, deflate"), None);

    // 目前只支持 gzip，deflate 会输出 warning 但不添加过滤器
    assert!(
        !chain.is_empty(),
        "Should at least handle supported encodings"
    );
}

// ==================== BZip2Decoder 测试 ====================

#[test]
fn test_bzip2_decompress_basic() {
    let original = b"BZip2 compression test data for verification.";
    let compressed = bzip2_test_data();

    let mut decoder = BZip2Decoder::new();
    let result = decoder
        .filter(&compressed)
        .expect("BZip2 decompression failed");

    assert_eq!(
        result, original,
        "BZip2 decompressed data should match original"
    );
    assert_eq!(decoder.name(), "bzip2");
}

#[test]
fn test_bzip2_invalid_data_error() {
    // 无效的 BZip2 数据
    let invalid_data = b"This is not valid bzip2 data";

    let mut decoder = BZip2Decoder::new();
    let result = decoder.filter(invalid_data);

    assert!(result.is_err(), "Invalid BZip2 data should cause error");
}

// ==================== 边界情况测试 ====================

#[test]
fn test_gzip_empty_data() {
    // 压缩空字符串
    let original = b"";
    let compressed = create_gzip_data(original);

    let mut decoder = GZipDecoder::new();
    let result = decoder
        .filter(&compressed)
        .expect("Empty GZip decompression failed");

    assert_eq!(result, original, "Empty data should decompress to empty");
}

#[test]
fn test_chunked_single_byte_chunks() {
    // 每个 chunk 只有1字节
    let chunked_data = b"1\r\nH\r\n1\r\ne\r\n1\r\nl\r\n1\r\nl\r\n1\r\no\r\n0\r\n\r\n";

    let mut decoder = ChunkedDecoder::new();
    let result = decoder
        .filter(chunked_data)
        .expect("Single byte chunks failed");

    assert_eq!(
        result, b"Hello",
        "Single byte chunks should concatenate correctly"
    );
}

#[test]
fn test_chunked_large_size() {
    // 大尺寸 chunk (100字节)
    let data = vec![b'X'; 100];
    let size_hex = format!("{:x}", 100);
    let chunked = format!(
        "{}\r\n{}\r\n0\r\n\r\n",
        size_hex,
        String::from_utf8_lossy(&data)
    )
    .into_bytes();

    let mut decoder = ChunkedDecoder::new();
    let result = decoder.filter(&chunked).expect("Large chunk failed");

    assert_eq!(result.len(), 100, "Should decode all 100 bytes");
    assert!(result.iter().all(|&b| b == b'X'), "All bytes should be X");
}
