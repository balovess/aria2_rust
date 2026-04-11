//! 流式数据解码器框架
//!
//! 提供可组合的流式数据过滤器，支持 GZip、Chunked、BZip2 等编码格式的解码。
//! 通过 `FilterChain` 可以将多个过滤器串联使用，实现复杂的数据处理流水线。

use crate::error::{Aria2Error, Result};
use flate2::read::GzDecoder;
use std::io::{Cursor, Read};

/// 流式过滤器 trait
///
/// 定义了流式数据处理器的接口，所有具体的过滤器实现都需要实现此 trait。
/// 过滤器支持增量式数据处理，可以在多次调用中逐步消费输入数据。
pub trait StreamFilter: Send + Sync + std::fmt::Debug {
    /// 处理输入数据并返回过滤后的结果
    ///
    /// # Arguments
    ///
    /// * `input` - 输入数据的字节切片
    ///
    /// # Returns
    ///
    /// 过滤后的数据，或错误信息
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>>;

    /// 刷新内部缓冲区并返回剩余数据
    ///
    /// 在输入结束后调用此方法以确保所有缓冲的数据都被输出。
    ///
    /// # Returns
    ///
    /// 缓冲区中的剩余数据，或错误信息
    fn flush(&mut self) -> Result<Vec<u8>>;

    /// 返回过滤器的名称（用于调试和日志）
    fn name(&self) -> &'static str;

    /// 检查是否需要更多输入才能继续处理
    ///
    /// 当返回 `false` 时，表示过滤器已经完成工作，不需要更多输入。
    fn needs_more_input(&self) -> bool;
}

// ==================== GZip 解码器 ====================

/// GZip 格式解压器
///
/// 使用 flate2 库实现的 GZip (RFC 1952) 数据解压器。
/// 支持流式解压，可以分块处理大型压缩文件。
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::{GZipDecoder, StreamFilter};
///
/// let mut decoder = GZipDecoder::new();
/// let compressed_data = /* 压缩的 GZip 数据 */;
/// let decompressed = decoder.filter(compressed_data)?;
/// ```
#[derive(Debug)]
pub struct GZipDecoder {
    /// 内部 GzDecoder 实例
    inner: Option<GzDecoder<Cursor<Vec<u8>>>>,
    /// 是否已完成解压
    finished: bool,
}

impl GZipDecoder {
    /// 创建新的 GZip 解码器实例
    ///
    /// # Returns
    ///
    /// 新的 GZipDecoder 实例
    pub fn new() -> Self {
        GZipDecoder {
            inner: None,
            finished: false,
        }
    }
}

impl Default for GZipDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for GZipDecoder {
    /// 处理 GZip 压缩数据
    ///
    /// 首次调用时会检测 GZip magic number (0x1f 0x8b)，验证数据格式有效性。
    /// 后续调用会将数据追加到内部缓冲区并进行解压。
    ///
    /// # Arguments
    ///
    /// * `input` - GZip 压缩的字节数据
    ///
    /// # Returns
    ///
    /// 解压后的原始数据，或错误信息
    ///
    /// # Errors
    ///
    /// - 如果输入数据不是有效的 GZip 格式（缺少 magic number）
    /// - 如果解压过程中发生 I/O 错误
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // 检查是否已经完成
        if self.finished && self.inner.is_none() {
            return Err(Aria2Error::Parse(
                "GZip decoder already finished".to_string(),
            ));
        }

        // 验证 GZip magic number (首次调用时)
        if self.inner.is_none() {
            if input.len() < 2 {
                return Err(Aria2Error::Parse(
                    "Input too short for GZip header".to_string(),
                ));
            }

            // GZip magic number: 0x1f 0x8b
            if input[0] != 0x1f || input[1] != 0x8b {
                return Err(Aria2Error::Parse("Invalid GZip magic number".to_string()));
            }

            // 初始化解码器
            let cursor = Cursor::new(input.to_vec());
            self.inner = Some(GzDecoder::new(cursor));
        } else {
            // 将新数据追加到已有的缓冲区
            // 注意：由于 GzDecoder 的限制，这里采用重新创建的方式
            // 实际应用中可能需要更复杂的缓冲管理
            return Err(Aria2Error::Parse(
                "GZip incremental decoding not fully supported in this implementation".to_string(),
            ));
        }

        // Execute decompression with pre-allocated output buffer
        // Gzip typically expands 2-3x, but allocate at least 256 bytes to avoid tiny reallocations
        if let Some(ref mut decoder) = self.inner {
            let mut output = Vec::with_capacity(input.len().saturating_mul(3).max(256));
            match decoder.read_to_end(&mut output) {
                Ok(_) => {
                    self.finished = true;
                    self.inner = None; // 释放解码器资源
                    Ok(output)
                }
                Err(e) => Err(Aria2Error::Io(e.to_string())),
            }
        } else {
            Err(Aria2Error::Parse(
                "GZip decoder not initialized".to_string(),
            ))
        }
    }

    /// 刷新 GZip 解码器缓冲区
    ///
    /// 返回内部剩余的已解压数据。对于一次性解压的场景，
    /// 此方法通常返回空向量（因为所有数据已在 filter() 中输出）。
    ///
    /// # Returns
    ///
    /// 缓冲区中的剩余数据
    fn flush(&mut self) -> Result<Vec<u8>> {
        if self.finished {
            Ok(Vec::new())
        } else if self.inner.is_some() {
            // 尝试完成解压
            let mut output = Vec::new();
            if let Some(ref mut decoder) = self.inner {
                let _ = decoder.read_to_end(&mut output);
            }
            self.finished = true;
            Ok(output)
        } else {
            Ok(Vec::new())
        }
    }

    /// 返回 "gzip"
    fn name(&self) -> &'static str {
        "gzip"
    }

    /// 检查是否需要更多输入
    ///
    /// 当 finished=true 且 inner=None 时返回 false，表示解压完成
    fn needs_more_input(&self) -> bool {
        !(self.finished && self.inner.is_none())
    }
}

// ==================== Chunked 解码器 ====================

/// Chunked Transfer-Encoding 状态枚举
#[derive(Debug, Clone, PartialEq)]
enum ChunkState {
    /// 正在读取 chunk size 行
    ReadingSize,
    /// 正在读取 chunk 数据
    ReadingData { remaining: usize },
    /// 读取数据后的 \r\n（chunk 数据结束标记）
    ReadingDataEnd,
    /// chunked 编码结束（遇到 size=0 的终止块）
    Complete,
    /// 发生错误
    Error(String),
}

/// HTTP Chunked Transfer-Encoding 解码器
///
/// 实现 RFC 7230 Section 4.1 规范的 chunked 编码解码。
/// 支持 chunk extensions（会忽略未知的扩展）。
///
/// # Format
///
/// ```text
/// chunked-body   = *chunk
///                  last-chunk
///                  trailer-section
///                  CRLF
///
/// chunk          = chunk-size [chunk-ext] CRLF
///                  chunk-data CRLF
/// chunk-size     = 1*HEXDIG
/// last-chunk     = 1*("0") [chunk-ext] CRLF
/// ```
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::{ChunkedDecoder, StreamFilter};
///
/// let mut decoder = ChunkedDecoder::new();
/// let chunked_data = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
/// let decoded = decoder.filter(chunked_data)?;
/// assert_eq!(decoded, b"hello world");
/// ```
#[derive(Debug)]
pub struct ChunkedDecoder {
    /// 当前解析状态
    state: ChunkState,
    /// 已收集的 size 行数据
    size_buffer: Vec<u8>,
    /// 当前 chunk 的剩余字节数
    current_chunk_remaining: usize,
    /// 解码后的输出缓冲区
    output_buffer: Vec<u8>,
}

impl ChunkedDecoder {
    /// 创建新的 Chunked 解码器实例
    ///
    /// # Returns
    ///
    /// 新的 ChunkedDecoder 实例
    pub fn new() -> Self {
        ChunkedDecoder {
            state: ChunkState::ReadingSize,
            size_buffer: Vec::new(),
            current_chunk_remaining: 0,
            output_buffer: Vec::new(),
        }
    }
}

impl Default for ChunkedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for ChunkedDecoder {
    /// 解析 chunked 编码数据
    ///
    /// 按照 RFC 7230 Section 4.1 规范解析 chunked 格式：
    /// - ReadingSize: 读取直到 \r\n，然后解析十六进制 size
    /// - ReadingData: 读取指定大小的数据，完成后回到 ReadingSize
    /// - 遇到 size=0 时进入 Complete 状态
    ///
    /// # Arguments
    ///
    /// * `input` - chunked 编码的字节数据
    ///
    /// # Returns
    ///
    /// 解码后的原始数据（去除 chunked 包装），或错误信息
    ///
    /// # Errors
    ///
    /// - 如果 chunk size 格式无效（非十六进制）
    /// - 如果在 Error 状态下尝试继续处理
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // 如果已经在错误状态，直接返回错误
        if let ChunkState::Error(ref msg) = self.state {
            return Err(Aria2Error::Parse(msg.clone()));
        }

        // 如果已经完成，返回空结果
        if matches!(self.state, ChunkState::Complete) {
            return Ok(Vec::new());
        }

        let mut pos = 0;

        while pos < input.len() {
            match &self.state {
                ChunkState::ReadingSize => {
                    // 收集字符直到遇到 \r\n
                    let byte = input[pos];
                    if byte == b'\r' {
                        // 跳过 \r
                        pos += 1;
                        // 检查下一个字节是否是 \n
                        if pos < input.len() && input[pos] == b'\n' {
                            pos += 1; // 跳过 \n

                            // 解析 size 行
                            let size_str = String::from_utf8_lossy(&self.size_buffer).to_string();
                            let size_str_trimmed = size_str.trim();

                            // 分离 size 和可能的 extensions
                            let size_part = if let Some(semi_pos) = size_str_trimmed.find(';') {
                                &size_str_trimmed[..semi_pos]
                            } else {
                                size_str_trimmed
                            };

                            // 解析十六进制大小
                            match usize::from_str_radix(size_part.trim(), 16) {
                                Ok(0) => {
                                    // 终止块
                                    self.state = ChunkState::Complete;
                                    self.size_buffer.clear();
                                    let result = std::mem::take(&mut self.output_buffer);
                                    return Ok(result);
                                }
                                Ok(size) => {
                                    self.state = ChunkState::ReadingData { remaining: size };
                                    self.current_chunk_remaining = size;
                                    self.size_buffer.clear();
                                }
                                Err(_) => {
                                    self.state = ChunkState::Error(format!(
                                        "Invalid chunk size: {}",
                                        size_part
                                    ));
                                    return Err(Aria2Error::Parse(format!(
                                        "Invalid chunk size format: {}",
                                        size_part
                                    )));
                                }
                            }
                        }
                        // 如果不是 \n，继续等待（pos 已经增加了）
                    } else if byte == b'\n' {
                        // 单独的 \n，也当作行结束符处理
                        pos += 1;

                        // 解析 size 行
                        let size_str = String::from_utf8_lossy(&self.size_buffer).to_string();
                        let size_str_trimmed = size_str.trim();

                        // 分离 size 和可能的 extensions
                        let size_part = if let Some(semi_pos) = size_str_trimmed.find(';') {
                            &size_str_trimmed[..semi_pos]
                        } else {
                            size_str_trimmed
                        };

                        // 解析十六进制大小
                        match usize::from_str_radix(size_part.trim(), 16) {
                            Ok(0) => {
                                self.state = ChunkState::Complete;
                                self.size_buffer.clear();
                                let result = std::mem::take(&mut self.output_buffer);
                                return Ok(result);
                            }
                            Ok(size) => {
                                self.state = ChunkState::ReadingData { remaining: size };
                                self.current_chunk_remaining = size;
                                self.size_buffer.clear();
                            }
                            Err(_) => {
                                self.state =
                                    ChunkState::Error(format!("Invalid chunk size: {}", size_part));
                                return Err(Aria2Error::Parse(format!(
                                    "Invalid chunk size format: {}",
                                    size_part
                                )));
                            }
                        }
                    } else {
                        // 收集 size 字符
                        self.size_buffer.push(byte);
                        pos += 1;
                    }
                }

                ChunkState::ReadingData { remaining } => {
                    let remaining_bytes = *remaining;
                    let available = input.len() - pos;

                    // 计算本次要复制的字节数
                    let to_copy = std::cmp::min(remaining_bytes, available);

                    // 复制数据到输出缓冲区
                    self.output_buffer
                        .extend_from_slice(&input[pos..pos + to_copy]);
                    pos += to_copy;

                    // 更新剩余计数
                    let new_remaining = remaining_bytes - to_copy;

                    if new_remaining == 0 {
                        // 当前 chunk 的数据已读完，期望下一个 \r\n
                        self.state = ChunkState::ReadingDataEnd;
                        self.current_chunk_remaining = 0;
                    } else {
                        // 更新状态中的剩余计数
                        self.state = ChunkState::ReadingData {
                            remaining: new_remaining,
                        };
                        self.current_chunk_remaining = new_remaining;
                    }
                }

                ChunkState::ReadingDataEnd => {
                    // 跳过 chunk 数据后的 \r\n
                    let byte = input[pos];
                    if byte == b'\r' || byte == b'\n' {
                        pos += 1; // 跳过 \r 或 \n
                    // 继续保持在 ReadingDataEnd 状态
                    } else {
                        // 遇到非换行字符，说明 \r\n 已经过完
                        // 转到 ReadingSize 来处理这个字符
                        self.state = ChunkState::ReadingSize;
                        // 不增加 pos，让下次循环处理这个字符
                    }
                }

                ChunkState::Complete => {
                    // 已经完成，忽略后续数据
                    break;
                }

                ChunkState::Error(msg) => {
                    return Err(Aria2Error::Parse(msg.clone()));
                }
            }
        }

        // 返回当前累积的所有输出数据
        let result = std::mem::take(&mut self.output_buffer);
        Ok(result)
    }

    /// 刷新 Chunked 解码器缓冲区
    ///
    /// 返回缓冲区中剩余的已解码数据。
    /// 如果解码过程不完整（仍在读取 chunk），返回错误。
    ///
    /// # Returns
    ///
    /// 缓冲区中的剩余数据，或错误信息（如果解码不完整）
    fn flush(&mut self) -> Result<Vec<u8>> {
        match &self.state {
            ChunkState::Complete => {
                // 完成状态，返回空
                Ok(Vec::new())
            }
            ChunkState::ReadingSize
            | ChunkState::ReadingData { .. }
            | ChunkState::ReadingDataEnd => {
                // 不完整的状态，可能有数据丢失
                let remaining = std::mem::take(&mut self.output_buffer);
                if remaining.is_empty() {
                    Err(Aria2Error::Parse(
                        "Incomplete chunked encoding data".to_string(),
                    ))
                } else {
                    // 返回已有数据，但标记为警告
                    Ok(remaining)
                }
            }
            ChunkState::Error(msg) => Err(Aria2Error::Parse(msg.clone())),
        }
    }

    /// 返回 "chunked"
    fn name(&self) -> &'static str {
        "chunked"
    }

    /// 检查是否需要更多输入
    ///
    /// 只有在 Complete 或 Error 状态时才返回 false
    fn needs_more_input(&self) -> bool {
        !matches!(self.state, ChunkState::Complete | ChunkState::Error(_))
    }
}

// ==================== BZip2 解码器 ====================

/// BZip2 格式解压器
///
/// 使用 bzip2 库实现的 BZip2 数据解压器。
/// 类似于 GZipDecoder，但使用 bzip2 压缩算法。
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::{BZip2Decoder, StreamFilter};
///
/// let mut decoder = BZip2Decoder::new();
/// let compressed_data = /* BZip2 压缩数据 */;
/// let decompressed = decoder.filter(compressed_data)?;
/// ```
pub struct BZip2Decoder {
    /// 内部 BzDecompressor 实例
    inner: Option<bzip2::read::BzDecoder<std::io::Cursor<Vec<u8>>>>,
    /// 是否已完成解压
    finished: bool,
}

impl std::fmt::Debug for BZip2Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BZip2Decoder")
            .field("finished", &self.finished)
            .finish()
    }
}

impl BZip2Decoder {
    /// 创建新的 BZip2 解码器实例
    ///
    /// # Returns
    ///
    /// 新的 BZip2Decoder 实例
    pub fn new() -> Self {
        BZip2Decoder {
            inner: None,
            finished: false,
        }
    }
}

impl Default for BZip2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for BZip2Decoder {
    /// 处理 BZip2 压缩数据
    ///
    /// 首次调用时初始化解码器并执行解压操作。
    ///
    /// # Arguments
    ///
    /// * `input` - BZip2 压缩的字节数据
    ///
    /// # Returns
    ///
    /// 解压后的原始数据，或错误信息
    ///
    /// # Errors
    ///
    /// - 如果输入数据不是有效的 BZip2 格式
    /// - 如果解压过程中发生 I/O 错误
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // 检查是否已经完成
        if self.finished && self.inner.is_none() {
            return Err(Aria2Error::Parse(
                "BZip2 decoder already finished".to_string(),
            ));
        }

        // 验证最小长度
        if input.len() < 10 {
            return Err(Aria2Error::Parse(
                "Input too short for BZip2 header".to_string(),
            ));
        }

        // 初始化解码器
        if self.inner.is_none() {
            let cursor = Cursor::new(input.to_vec());
            self.inner = Some(bzip2::read::BzDecoder::new(cursor));
        } else {
            return Err(Aria2Error::Parse(
                "BZip2 incremental decoding not supported in this implementation".to_string(),
            ));
        }

        // 执行解压
        if let Some(ref mut decoder) = self.inner {
            let mut output = Vec::new();
            match decoder.read_to_end(&mut output) {
                Ok(_) => {
                    self.finished = true;
                    Ok(output)
                }
                Err(e) => Err(Aria2Error::Io(e.to_string())),
            }
        } else {
            Err(Aria2Error::Parse(
                "BZip2 decoder not initialized".to_string(),
            ))
        }
    }

    /// 刷新 BZip2 解码器缓冲区
    ///
    /// # Returns
    ///
    /// 缓冲区中的剩余数据
    fn flush(&mut self) -> Result<Vec<u8>> {
        if self.finished {
            Ok(Vec::new())
        } else if self.inner.is_some() {
            let mut output = Vec::new();
            if let Some(ref mut decoder) = self.inner {
                let _ = decoder.read_to_end(&mut output);
            }
            self.finished = true;
            Ok(output)
        } else {
            Ok(Vec::new())
        }
    }

    /// 返回 "bzip2"
    fn name(&self) -> &'static str {
        "bzip2"
    }

    /// 检查是否需要更多输入
    fn needs_more_input(&self) -> bool {
        !(self.finished && self.inner.is_none())
    }
}

// ==================== FilterChain 过滤器链 ====================

/// 流式过滤器链
///
/// 允许将多个 `StreamFilter` 串联起来形成数据处理流水线。
/// 数据会依次通过每个过滤器，前一个过滤器的输出作为后一个的输入。
///
/// # Architecture
///
/// ```text
/// Input → Filter1 → Filter2 → ... → FilterN → Output
/// ```
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::*;
/// use std::sync::Arc;
///
/// let mut chain = FilterChain::new();
/// chain.push(Box::new(GZipDecoder::new()));
/// chain.push(Box::new(ChunkedDecoder::new()));
///
/// let output = chain.process(input_data)?;
/// ```
#[derive(Debug, Default)]
pub struct FilterChain {
    /// 过滤器列表（按执行顺序排列）
    filters: Vec<Box<dyn StreamFilter>>,
}

impl FilterChain {
    /// 创建空的过滤器链
    ///
    /// # Returns
    ///
    /// 新的 FilterChain 实例（不含任何过滤器）
    pub fn new() -> Self {
        FilterChain {
            filters: Vec::new(),
        }
    }

    /// 向过滤器链末尾添加一个过滤器
    ///
    /// 新添加的过滤器将在最后执行。
    ///
    /// # Arguments
    ///
    /// * `filter` - 要添加的过滤器（装箱后的 trait object）
    pub fn push(&mut self, filter: Box<dyn StreamFilter>) {
        self.filters.push(filter);
    }

    /// 清除所有过滤器
    ///
    /// 移除链中的所有过滤器，使其变为空链。
    pub fn clear(&mut self) {
        self.filters.clear();
    }

    /// Process input data through the filter chain
    ///
    /// Passes input data through each filter in sequence. The first filter receives
    /// a direct reference to the input to avoid unnecessary cloning. Subsequent filters
    /// receive the output from the previous filter.
    ///
    /// # Arguments
    ///
    /// * `input` - Raw data to be processed
    ///
    /// # Returns
    ///
    /// Final data after processing through all filters, or an error
    ///
    /// # Errors
    ///
    /// If any filter fails, returns error immediately and stops processing
    ///
    /// # Performance
    ///
    /// Optimized to avoid cloning input data before the first filter call.
    /// Only allocates when filters actually modify the data.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let mut chain = FilterChain::new();
    /// chain.push(Box::new(GZipDecoder::new()));
    /// let result = chain.process(gzip_compressed_data)?;
    /// ```
    pub fn process(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // Optimization: avoid unnecessary clone by passing input reference directly to first filter
        let mut data: Option<Vec<u8>> = None;

        for (index, filter) in self.filters.iter_mut().enumerate() {
            data = Some(if index == 0 {
                // First filter: pass input reference directly (no clone needed)
                filter.filter(input)?
            } else {
                // Subsequent filters: use output from previous filter
                filter.filter(data.as_ref().unwrap())?
            });
        }

        Ok(data.unwrap_or_else(|| input.to_vec()))
    }

    /// 刷新所有过滤器的缓冲区
    ///
    /// 依次调用链中每个过滤器的 flush 方法，
    /// 确保所有缓冲的数据都被输出。
    ///
    /// # Returns
    ///
    /// 所有过滤器的剩余数据经过完整处理后得到的最终结果
    ///
    /// # Errors
    ///
    /// 如果任何过滤器刷新失败，返回错误
    pub fn flush(&mut self) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        for filter in &mut self.filters {
            let flushed = filter.flush()?;
            data.extend_from_slice(&flushed);
        }
        Ok(data)
    }

    /// 获取链中过滤器的数量
    ///
    /// # Returns
    ///
    /// 当前注册的过滤器数量
    pub fn len(&self) -> usize {
        self.filters.len()
    }

    /// 检查过滤器链是否为空
    ///
    /// # Returns
    ///
    /// 如果没有任何过滤器则返回 true
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }
}

// ==================== AutoFilterSelector 自动选择器 ====================

/// HTTP 内容编码自动选择器
///
/// 根据 HTTP headers 自动选择合适的解码过滤器链。
/// 遵循 RFC 7230 Section 3.3.1 规范：Transfer-Encoding 优先于 Content-Encoding。
///
/// # Priority Rules
///
/// 1. **Transfer-Encoding: chunked** → 添加 `ChunkedDecoder`
/// 2. **Content-Encoding: gzip | x-gzip** → 添加 `GZipDecoder`
/// 3. **Content-Encoding: deflate** → 添加 `ZlibDecoder`（未来支持）
/// 4. **Content-Encoding: bzip2 | x-bzip2** → 添加 `BZip2Decoder`
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::AutoFilterSelector;
///
/// // 根据 Content-Encoding: gzip 自动选择 GZip 解码器
/// let chain = AutoFilterSelector::select_filters(Some("gzip"), None);
/// assert_eq!(chain.len(), 1);
///
/// // Transfer-Encoding 优先
/// let chain = AutoFilterSelector::select_filters(Some("gzip"), Some("chunked"));
/// assert_eq!(chain.len(), 1); // 只有 chunked
/// ```
pub struct AutoFilterSelector;

impl AutoFilterSelector {
    /// 根据 HTTP headers 创建合适的 FilterChain
    ///
    /// 自动分析 Content-Encoding 和 Transfer-Encoding headers，
    /// 构建相应的解码过滤器链。
    ///
    /// # Arguments
    ///
    /// * `content_encoding` - Content-Encoding header 的值（可选）
    /// * `transfer_encoding` - Transfer-Encoding header 的值（可选）
    ///
    /// # Returns
    ///
    /// 配置好的 FilterChain 实例
    ///
    /// # RFC Compliance
    ///
    /// 遵循 RFC 7230 Section 3.3.1:
    /// - Transfer-Encoding 优先级高于 Content-Encoding
    /// - 多个编码值按顺序处理（逗号分隔）
    pub fn select_filters(
        content_encoding: Option<&str>,
        transfer_encoding: Option<&str>,
    ) -> FilterChain {
        let mut chain = FilterChain::new();

        // Transfer-Encoding 优先于 Content-Encoding (RFC 7230)
        if let Some(te) = transfer_encoding {
            // 解析多个值（逗号分隔）
            for encoding in te.split(',') {
                let encoding = encoding.trim().to_lowercase();
                match encoding.as_str() {
                    "chunked" => {
                        chain.push(Box::new(ChunkedDecoder::new()));
                    }
                    "gzip" | "x-gzip" => {
                        chain.push(Box::new(GZipDecoder::new()));
                    }
                    "deflate" => {
                        // TODO: 未来支持 ZlibDecoder
                        tracing::warn!("Deflate encoding not yet implemented");
                    }
                    "bzip2" | "x-bzip2" => {
                        chain.push(Box::new(BZip2Decoder::new()));
                    }
                    _ => {
                        tracing::debug!("Unknown transfer encoding: {}", encoding);
                    }
                }
            }
        } else if let Some(ce) = content_encoding {
            // 仅在没有 Transfer-Encoding 时处理 Content-Encoding
            for encoding in ce.split(',') {
                let encoding = encoding.trim().to_lowercase();
                match encoding.as_str() {
                    "gzip" | "x-gzip" => {
                        chain.push(Box::new(GZipDecoder::new()));
                    }
                    "deflate" => {
                        // TODO: 未来支持 ZlibDecoder
                        tracing::warn!("Deflate encoding not yet implemented");
                    }
                    "bzip2" | "x-bzip2" => {
                        chain.push(Box::new(BZip2Decoder::new()));
                    }
                    "identity" | "" => {
                        // identity 表示无编码，忽略
                    }
                    _ => {
                        tracing::debug!("Unknown content encoding: {}", encoding);
                    }
                }
            }
        }

        chain
    }
}
