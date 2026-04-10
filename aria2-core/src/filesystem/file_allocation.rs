use super::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use crate::error::{Aria2Error, Result};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AllocationStrategy {
    None,
    Prealloc,
    Falloc,
    Trunc,
}

impl Default for AllocationStrategy {
    fn default() -> Self {
        AllocationStrategy::None
    }
}

impl AllocationStrategy {
    pub fn from_str(s: &str) -> Self {
        match s {
            "prealloc" => AllocationStrategy::Prealloc,
            "falloc" => AllocationStrategy::Falloc,
            "trunc" => AllocationStrategy::Trunc,
            _ => AllocationStrategy::None,
        }
    }
}

pub async fn allocate_file<D: DiskAdaptor>(
    adaptor: &mut D,
    _path: &Path,
    length: u64,
    strategy: AllocationStrategy,
) -> Result<()> {
    match strategy {
        AllocationStrategy::None => Ok(()),
        AllocationStrategy::Prealloc => preallocate(adaptor, length).await,
        AllocationStrategy::Falloc => fallocate(adaptor, length).await,
        AllocationStrategy::Trunc => truncate(adaptor, length).await,
    }
}

pub async fn preallocate_file(path: &Path, length: u64, strategy: &str) -> Result<()> {
    let alloc_strategy = AllocationStrategy::from_str(strategy);

    if length == 0 || alloc_strategy == AllocationStrategy::None {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        let parent: &Path = parent;
        if !parent.exists() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e: std::io::Error| Aria2Error::Io(e.to_string()))?;
        }
    }

    let mut adaptor = DirectDiskAdaptor::new();
    adaptor.open(path).await?;
    allocate_file(&mut adaptor, path, length, alloc_strategy).await?;
    adaptor.close().await
}

async fn preallocate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    adaptor.truncate(length).await
}

async fn fallocate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    #[cfg(unix)]
    {
        if let Some(fd) = adaptor.unix_raw_fd() {
            unsafe {
                let ret = libc::posix_fallocate64(fd, 0, length as i64);
                if ret != 0 {
                    return Err(Aria2Error::Io(
                        std::io::Error::from_raw_os_error(ret).to_string(),
                    ));
                }
            }
        } else {
            adaptor.truncate(length).await?
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        adaptor.truncate(length).await
    }
}

async fn truncate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    adaptor.truncate(length).await
}

pub async fn get_available_space(path: &Path) -> Result<u64> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = tokio::fs::metadata(parent)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        let statvfs_result = unsafe {
            let mut stat: libc::statvfs64 = std::mem::zeroed();
            let ret = libc::statvfs64(
                parent.to_str().unwrap_or(".").as_ptr() as *const i8,
                &mut stat,
            );
            (ret, stat)
        };

        if statvfs_result.0 == 0 {
            let stat = statvfs_result.1;
            Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
        } else {
            Err(Aria2Error::Io("无法获取磁盘空间".to_string()))
        }
    }

    #[cfg(windows)]
    {
        let metadata = tokio::fs::metadata(parent)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        let free = metadata.len();
        if free > 0 {
            Ok(free)
        } else {
            Ok(u64::MAX / 2)
        }
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        Ok(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocation_strategy_from_str() {
        assert_eq!(
            AllocationStrategy::from_str("none"),
            AllocationStrategy::None
        );
        assert_eq!(
            AllocationStrategy::from_str("prealloc"),
            AllocationStrategy::Prealloc
        );
        assert_eq!(
            AllocationStrategy::from_str("falloc"),
            AllocationStrategy::Falloc
        );
        assert_eq!(
            AllocationStrategy::from_str("trunc"),
            AllocationStrategy::Trunc
        );
        assert_eq!(
            AllocationStrategy::from_str("invalid"),
            AllocationStrategy::None
        );
        assert_eq!(AllocationStrategy::from_str(""), AllocationStrategy::None);
    }

    #[tokio::test]
    async fn test_preallocate_file_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_none.bin");
        preallocate_file(&path, 1024, "none").await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_preallocate_file_trunc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_trunc.bin");
        preallocate_file(&path, 4096, "trunc").await.unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 4096);
    }

    #[tokio::test]
    async fn test_preallocate_file_prealloc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_prealloc.bin");
        preallocate_file(&path, 1024 * 1024, "prealloc")
            .await
            .unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 1024 * 1024);
    }

    #[tokio::test]
    async fn test_preallocate_zero_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_zero.bin");
        preallocate_file(&path, 0, "trunc").await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_preallocate_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub1").join("sub2").join("test_nested.bin");
        preallocate_file(&path, 100, "trunc").await.unwrap();

        assert!(path.exists());
        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 100);
    }

    #[tokio::test]
    async fn test_get_available_space_returns_value() {
        let dir = tempfile::tempdir().unwrap();
        let space = get_available_space(dir.path()).await;
        assert!(space.is_ok());
        let val = space.unwrap();
        assert!(val > 0);
    }

    #[tokio::test]
    async fn test_preallocate_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_overwrite.bin");

        tokio::fs::write(&path, b"original data").await.unwrap();
        preallocate_file(&path, 2048, "trunc").await.unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 2048);
    }
}
