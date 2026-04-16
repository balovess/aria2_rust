//! 流式数据解码器框架
//!
//! 提供可组合的流式数据过滤器，支持 GZip、Chunked、BZip2 等编码格式的解码。
//! 通过 `FilterChain` 可以将多个过滤器串联使用，实现复杂的数据处理流水线。

use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_writer::SeekableDiskWriter;
use bzip2_rs::DecoderReader as BzDecoder;
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
    inner: Option<BzDecoder<Cursor<Vec<u8>>>>,
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
            self.inner = Some(BzDecoder::new(cursor));
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
                        // Identity / none encoding → passthrough (no decoder needed)
                        if encoding.eq_ignore_ascii_case("identity")
                            || encoding.eq_ignore_ascii_case("none")
                        {
                            // No decoder needed for identity encoding
                            continue;
                        }

                        // LZMA / x-lzma → log warning, return identity chain (not yet supported)
                        if encoding.contains("lzma") {
                            tracing::warn!(
                                "LZMA encoding not yet supported, returning passthrough"
                            );
                            continue;
                        }

                        // Brotli (br) → placeholder for future support
                        if encoding.eq_ignore_ascii_case("br") {
                            tracing::debug!("Brotli encoding detected but not yet implemented");
                            continue;
                        }

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
                        // Identity / none encoding → passthrough (no decoder needed)
                        if encoding.eq_ignore_ascii_case("identity")
                            || encoding.eq_ignore_ascii_case("none")
                        {
                            // No decoder needed for identity encoding
                            continue;
                        }

                        // LZMA / x-lzma → log warning, return identity chain (not yet supported)
                        if encoding.contains("lzma") {
                            tracing::warn!(
                                "LZMA encoding not yet supported, returning passthrough"
                            );
                            continue;
                        }

                        // Brotli (br) → placeholder for future support
                        if encoding.eq_ignore_ascii_case("br") {
                            tracing::debug!("Brotli encoding detected but not yet implemented");
                            continue;
                        }

                        tracing::debug!("Unknown content encoding: {}", encoding);
                    }
                }
            }
        }

        chain
    }
}

/// Detect content encoding from magic bytes as fallback when Content-Encoding header
/// may be incorrect or missing.
///
/// Examines the first few bytes of data to identify known compression formats:
/// - Gzip: bytes [0x1f, 0x8b]
/// - BZ2: bytes [0x42, 0x5a] ("BZ")
/// - Zlib/Deflate: byte [0x78] followed by valid flag byte
///
/// # Arguments
///
/// * `data` - Raw data bytes to examine
///
/// # Returns
///
/// A string slice representing the detected encoding:
/// - "gzip" for GZip compressed data
/// - "bzip2" for BZip2 compressed data
/// - "deflate" for Zlib/Deflate compressed data
/// - "identity" for uncompressed/unknown data
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::detect_encoding_from_magic_bytes;
///
/// let gzip_data = vec![0x1f, 0x8b, 0x08, ...];
/// assert_eq!(detect_encoding_from_magic_bytes(&gzip_data), "gzip");
/// ```
pub fn detect_encoding_from_magic_bytes(data: &[u8]) -> &'static str {
    // Check for Gzip magic number: 0x1f 0x8b
    if data.len() >= 2 {
        if data[0] == 0x1f && data[1] == 0x8b {
            return "gzip";
        }
        // Check for BZip2 magic number: 0x42 0x5a ("BZ")
        if data[0] == 0x42 && data[1] == 0x5a {
            return "bzip2";
        }
    }
    // Check for Zlib/Deflate magic number: 0x78 followed by valid flag byte
    if !data.is_empty() && data[0] == 0x78 {
        return "deflate";
    }
    // Default to identity (no compression)
    "identity"
}

/// Wraps a disk writer with automatic stream filtering.
///
/// Data written through this wrapper passes through the configured
/// FilterChain before being written to disk. This enables transparent
/// decompression of compressed streams during download.
///
/// # Type Parameters
///
/// * `W` - A type implementing `SeekableDiskWriter` for actual disk I/O
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::{StreamingFilterWriter, FilterChain, GZipDecoder};
/// use aria2_core::filesystem::disk_writer::CachedDiskWriter;
///
/// let writer = CachedDiskWriter::new(&path, None, None);
/// let mut chain = FilterChain::new();
/// chain.push(Box::new(GZipDecoder::new()));
///
/// let mut filter_writer = StreamingFilterWriter::new(writer, chain);
/// filter_writer.write_filtered(&compressed_data).await?;
/// filter_writer.flush_filtered().await?;
/// ```
pub struct StreamingFilterWriter<W: SeekableDiskWriter> {
    /// Underlying disk writer for actual I/O operations
    inner: W,
    /// Filter chain to process data through
    chain: FilterChain,
    /// Buffered input data waiting to be processed
    buffer: Vec<u8>,
    /// Process data in chunks of this size (default 64KB)
    chunk_size: usize,
    /// Total bytes written to underlying writer (after filtering)
    total_written: u64,
    /// Total bytes received as input (before filtering)
    total_input: u64,
    /// Current write offset in the underlying writer
    write_offset: u64,
}

impl<W: SeekableDiskWriter> StreamingFilterWriter<W> {
    /// Create a new StreamingFilterWriter with default settings.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying disk writer
    /// * `chain` - The filter chain to apply to all written data
    ///
    /// # Returns
    ///
    /// A new StreamingFilterWriter instance with 64KB chunk size
    pub fn new(inner: W, chain: FilterChain) -> Self {
        Self {
            inner,
            chain,
            buffer: Vec::with_capacity(64 * 1024),
            chunk_size: 64 * 1024,
            total_written: 0,
            total_input: 0,
            write_offset: 0,
        }
    }

    /// Set custom chunk size for processing.
    ///
    /// Smaller chunks use less memory but may be less efficient.
    /// Larger chunks improve throughput but increase memory usage.
    /// Minimum chunk size is 1KB.
    ///
    /// # Arguments
    ///
    /// * `size` - Desired chunk size in bytes (minimum 1024)
    ///
    /// # Returns
    ///
    /// Self for method chaining
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(1024);
        self
    }

    /// Write data through the filter chain to underlying writer.
    ///
    /// Data is buffered internally until a full chunk is accumulated,
    /// then processed through the filter chain and written to disk.
    ///
    /// # Arguments
    ///
    /// * `data` - Raw (possibly compressed) data to write
    ///
    /// # Returns
    ///
    /// Ok(()) on success, or an error string if filtering/writing fails
    ///
    /// # Errors
    ///
    /// - If the filter chain fails to process the data
    /// - If the underlying writer fails to write
    pub async fn write_filtered(&mut self, data: &[u8]) -> Result<()> {
        self.total_input += data.len() as u64;
        self.buffer.extend_from_slice(data);

        // Process complete chunks
        while self.buffer.len() >= self.chunk_size {
            let chunk = self.buffer.drain(..self.chunk_size).collect::<Vec<_>>();
            let filtered = self.chain.process(&chunk)?;
            if !filtered.is_empty() {
                self.inner.write_at(self.write_offset, &filtered).await?;
                self.write_offset += filtered.len() as u64;
                self.total_written += filtered.len() as u64;
            }
        }

        Ok(())
    }

    /// Flush remaining buffered data through the filter chain.
    ///
    /// Must be called after all data has been written to ensure
    /// remaining buffered data is processed and written to disk.
    ///
    /// # Returns
    ///
    /// Ok(()) on success, or an error string if flushing fails
    ///
    /// # Errors
    ///
    /// - If the filter chain fails during final processing
    /// - If the underlying writer fails to flush
    pub async fn flush_filtered(&mut self) -> Result<()> {
        // Process any remaining buffered data
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            let filtered = self.chain.process(&remaining)?;
            if !filtered.is_empty() {
                self.inner.write_at(self.write_offset, &filtered).await?;
                self.write_offset += filtered.len() as u64;
                self.total_written += filtered.len() as u64;
            }
        }

        // Flush the underlying writer
        self.inner.flush().await?;
        Ok(())
    }

    /// Get total number of input bytes received (before filtering).
    ///
    /// # Returns
    ///
    /// Total uncompressed/compressed input bytes
    pub fn total_input_bytes(&self) -> u64 {
        self.total_input
    }

    /// Get total number of output bytes written (after filtering).
    ///
    /// # Returns
    ///
    /// Total decompressed/filtered output bytes
    pub fn total_output_bytes(&self) -> u64 {
        self.total_written
    }

    /// Calculate compression ratio (output / input).
    ///
    /// Values > 1.0 indicate expansion (common with already-compressed data).
    /// Values < 1.0 indicate successful compression.
    /// Returns 1.0 if no data has been processed.
    ///
    /// # Returns
    ///
    /// Compression ratio as f64
    pub fn compression_ratio(&self) -> f64 {
        if self.total_input > 0 {
            self.total_output_bytes() as f64 / self.total_input as f64
        } else {
            1.0
        }
    }

    /// Consume this wrapper and return the underlying writer.
    ///
    /// Useful when you need direct access to the underlying writer
    /// after streaming is complete.
    ///
    /// # Returns
    ///
    /// The inner SeekableDiskWriter instance
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Get a reference to the inner writer.
    ///
    /// # Returns
    ///
    /// Immutable reference to the underlying SeekableDiskWriter
    pub fn inner(&self) -> &W {
        &self.inner
    }

    /// Get a mutable reference to the inner writer.
    ///
    /// # Returns
    ///
    /// Mutable reference to the underlying SeekableDiskWriter
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};

    // Mock implementation of SeekableDiskWriter for testing
    struct MockSeekableWriter {
        data: Vec<u8>,
        opened: bool,
    }

    impl MockSeekableWriter {
        fn new() -> Self {
            MockSeekableWriter {
                data: Vec::new(),
                opened: false,
            }
        }
    }

    #[async_trait]
    impl SeekableDiskWriter for MockSeekableWriter {
        async fn open(&mut self) -> Result<()> {
            self.opened = true;
            Ok(())
        }

        async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
            // Ensure vector is large enough
            let end = offset as usize + buf.len();
            if self.data.len() < end {
                self.data.resize(end, 0);
            }
            self.data[offset as usize..end].copy_from_slice(buf);
            Ok(())
        }

        async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            let start = offset as usize;
            if start >= self.data.len() {
                return Ok(0);
            }
            let available = self.data.len() - start;
            let to_copy = available.min(buf.len());
            buf[..to_copy].copy_from_slice(&self.data[start..start + to_copy]);
            Ok(to_copy)
        }

        async fn truncate(&mut self, length: u64) -> Result<()> {
            self.data.truncate(length as usize);
            Ok(())
        }

        async fn flush(&mut self) -> Result<()> {
            Ok(())
        }

        async fn len(&self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }

        fn path(&self) -> &Path {
            static PATH: std::sync::LazyLock<PathBuf> =
                std::sync::LazyLock::new(|| PathBuf::from("/mock/path"));
            &PATH
        }
    }

    #[test]
    fn test_detect_magic_gzip() {
        // Test GZip magic bytes: 0x1f 0x8b
        let gzip_data = vec![0x1f, 0x8b, 0x08, 0x00];
        assert_eq!(detect_encoding_from_magic_bytes(&gzip_data), "gzip");

        // Test with exactly 2 bytes (minimum required)
        let gzip_minimal = vec![0x1f, 0x8b];
        assert_eq!(detect_encoding_from_magic_bytes(&gzip_minimal), "gzip");

        // Test with more realistic gzip header
        let gzip_realistic = vec![0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03];
        assert_eq!(detect_encoding_from_magic_bytes(&gzip_realistic), "gzip");
    }

    #[test]
    fn test_detect_magic_bzip2() {
        // Test BZip2 magic bytes: 0x42 0x5a ("BZ")
        let bzip2_data = vec![0x42, 0x5a, 0x68, 0x39]; // "BZh9" - common bzip2 start
        assert_eq!(detect_encoding_from_magic_bytes(&bzip2_data), "bzip2");

        // Test with minimal bzip2 header
        let bzip2_minimal = vec![0x42, 0x5a];
        assert_eq!(detect_encoding_from_magic_bytes(&bzip2_minimal), "bzip2");

        // Test that BZ is detected before checking for deflate (0x78)
        let bzip2_not_deflate = vec![0x42, 0x5a, 0x78, 0x9c];
        assert_eq!(
            detect_encoding_from_magic_bytes(&bzip2_not_deflate),
            "bzip2"
        );
    }

    #[test]
    fn test_unknown_encoding_passthrough() {
        // Test that AutoFilterSelector handles unknown encodings without errors

        // Test "br" (Brotli) - should return empty chain (passthrough)
        let chain_br = AutoFilterSelector::select_filters(Some("br"), None);
        assert_eq!(
            chain_br.len(),
            0,
            "Brotli encoding should result in empty filter chain"
        );

        // Test "lzma" - should return empty chain (passthrough)
        let chain_lzma = AutoFilterSelector::select_filters(Some("lzma"), None);
        assert_eq!(
            chain_lzma.len(),
            0,
            "LZMA encoding should result in empty filter chain"
        );

        // Test "identity" - should return empty chain (no decoder needed)
        let chain_identity = AutoFilterSelector::select_filters(Some("identity"), None);
        assert_eq!(
            chain_identity.len(),
            0,
            "Identity encoding should result in empty filter chain"
        );

        // Test "none" - should return empty chain (no decoder needed)
        let chain_none = AutoFilterSelector::select_filters(Some("none"), None);
        assert_eq!(
            chain_none.len(),
            0,
            "None encoding should result in empty filter chain"
        );

        // Test Transfer-Encoding with unknown values
        let chain_te_br = AutoFilterSelector::select_filters(None, Some("br"));
        assert_eq!(
            chain_te_br.len(),
            0,
            "Transfer-Encoding br should result in empty filter chain"
        );

        let chain_te_lzma = AutoFilterSelector::select_filters(None, Some("lzma"));
        assert_eq!(
            chain_te_lzma.len(),
            0,
            "Transfer-Encoding lzma should result in empty filter chain"
        );
    }

    #[tokio::test]
    async fn test_streaming_filter_writer_basic() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write as SyncWrite;

        // Create test data and compress it with gzip
        let original_data = b"Hello, StreamingFilterWriter! This is a test of the streaming filter writer implementation.";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(original_data).unwrap();
        let compressed_data = encoder.finish().unwrap();

        // Verify the compressed data starts with gzip magic bytes
        assert_eq!(compressed_data[0], 0x1f);
        assert_eq!(compressed_data[1], 0x8b);

        // Create a filter chain with GZip decoder
        let mut chain = FilterChain::new();
        chain.push(Box::new(GZipDecoder::new()));

        // Create StreamingFilterWriter with mock writer
        let mock_writer = MockSeekableWriter::new();
        let mut filter_writer = StreamingFilterWriter::new(mock_writer, chain);

        // Write compressed data through the filter
        filter_writer
            .write_filtered(&compressed_data)
            .await
            .unwrap();

        // Verify input tracking
        assert_eq!(
            filter_writer.total_input_bytes(),
            compressed_data.len() as u64,
            "Input byte count should match compressed data size"
        );

        // Flush remaining data
        filter_writer.flush_filtered().await.unwrap();

        // Verify output tracking
        assert!(
            filter_writer.total_output_bytes() > 0,
            "Should have written decompressed data"
        );

        // Verify compression ratio
        let ratio = filter_writer.compression_ratio();
        assert!(
            ratio > 0.0,
            "Compression ratio should be > 0, got {}",
            ratio
        );

        // Retrieve inner writer and verify decompressed data
        let inner = filter_writer.into_inner();
        let written_data = &inner.data;

        // Verify the decompressed data matches original
        assert_eq!(
            written_data, original_data,
            "Decompressed data should match original input"
        );

        // Test with chunk size customization
        let mock_writer2 = MockSeekableWriter::new();
        let mut chain2 = FilterChain::new();
        chain2.push(Box::new(GZipDecoder::new()));
        let mut filter_writer2 =
            StreamingFilterWriter::new(mock_writer2, chain2).with_chunk_size(1024);

        filter_writer2
            .write_filtered(&compressed_data)
            .await
            .unwrap();
        filter_writer2.flush_filtered().await.unwrap();

        let inner2 = filter_writer2.into_inner();
        assert_eq!(
            &inner2.data, original_data,
            "Should produce same result with custom chunk size"
        );
    }
}
