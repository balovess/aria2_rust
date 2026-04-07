use std::collections::VecDeque;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::debug;

use crate::error::Result;

#[derive(Clone)]
pub struct CacheEntry {
    offset: u64,
    data: Vec<u8>,
    dirty: bool,
    #[allow(dead_code)]
    last_access: Instant,
}

pub struct WrDiskCache {
    entries: Mutex<VecDeque<CacheEntry>>,
    max_size: usize,
    current_size: Mutex<usize>,
}

impl WrDiskCache {
    pub fn new(max_size_mb: usize) -> Self {
        let max_size = max_size_mb * 1024 * 1024;
        
        debug!("初始化写缓存, 最大容量: {} MB", max_size_mb);
        
        WrDiskCache {
            entries: Mutex::new(VecDeque::new()),
            max_size,
            current_size: Mutex::new(0),
        }
    }

    pub async fn write(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        let mut entries = self.entries.lock().await;
        let mut current_size = self.current_size.lock().await;
        
        let entry_size = data.len();
        
        if *current_size + entry_size > self.max_size {
            self.evict_if_needed(&mut entries, &mut current_size, entry_size).await;
        }
        
        let entry = CacheEntry {
            offset,
            data,
            dirty: true,
            last_access: Instant::now(),
        };
        
        entries.push_back(entry);
        *current_size += entry_size;
        
        debug!(
            "写入缓存, 偏移: {}, 大小: {}, 缓存使用: {}/{}",
            offset,
            entry_size,
            *current_size,
            self.max_size
        );
        
        Ok(())
    }

    pub async fn read(&self, offset: u64, length: u64) -> Result<Option<Vec<u8>>> {
        let entries = self.entries.lock().await;
        
        for entry in entries.iter() {
            if entry.offset == offset && entry.data.len() >= length as usize {
                return Ok(Some(entry.data[..length as usize].to_vec()));
            }
            
            if entry.offset <= offset 
                && (entry.offset + entry.data.len() as u64) >= (offset + length)
            {
                let start = (offset - entry.offset) as usize;
                let end = start + length as usize;
                if end <= entry.data.len() {
                    return Ok(Some(entry.data[start..end].to_vec()));
                }
            }
        }
        
        Ok(None)
    }

    pub async fn flush(&self) -> Result<Vec<CacheEntry>> {
        let mut entries = self.entries.lock().await;
        
        let flushed: Vec<CacheEntry> = entries.iter()
            .filter(|e| e.dirty)
            .cloned()
            .collect();
        
        for entry in entries.iter_mut() {
            entry.dirty = false;
        }
        
        debug!("刷新缓存，刷新条目数：{}", flushed.len());
        
        Ok(flushed)
    }

    pub async fn clear(&self) -> Result<()> {
        let mut entries = self.entries.lock().await;
        let mut current_size = self.current_size.lock().await;
        
        entries.clear();
        *current_size = 0;
        
        debug!("清除缓存");
        Ok(())
    }

    pub async fn size(&self) -> usize {
        *self.current_size.lock().await
    }

    pub async fn is_empty(&self) -> bool {
        self.size().await == 0
    }

    pub async fn count(&self) -> usize {
        self.entries.lock().await.len()
    }

    async fn evict_if_needed(
        &self,
        entries: &mut VecDeque<CacheEntry>,
        current_size: &mut usize,
        needed_size: usize,
    ) {
        while *current_size + needed_size > self.max_size {
            if let Some(entry) = entries.pop_front() {
                *current_size -= entry.data.len();
                debug!(
                    "淘汰缓存条目, 偏移: {}, 大小: {}",
                    entry.offset,
                    entry.data.len()
                );
            } else {
                break;
            }
        }
    }
}
