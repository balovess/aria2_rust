use std::path::Path;
use crate::error::{Aria2Error, Result};
use super::disk_adaptor::DiskAdaptor;

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

async fn preallocate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    adaptor.truncate(length).await
}

async fn fallocate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if let Some(file) = adaptor.as_any().downcast_ref::<std::fs::File>() {
            let fd = file.as_rawFd();
            unsafe {
                let ret = libc::posix_fallocate64(fd, 0, length as i64);
                if ret != 0 {
                    return Err(Aria2Error::Io(
                        std::io::Error::from_raw_os_error(ret).to_string()
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
    let parent = path.parent()
        .unwrap_or_else(|| Path::new("."));
    
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = tokio::fs::metadata(parent).await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        
        let statvfs_result = unsafe {
            let mut stat: libc::statvfs64 = std::mem::zeroed();
            let ret = libc::statvfs64(
                parent.to_str().unwrap_or(".").as_ptr() as *const i8,
                &mut stat
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
        let _ = tokio::fs::metadata(parent).await
            .map_err(|e| Aria2Error::Io(e.to_string()));
        
        Ok(u64::MAX)
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        Ok(u64::MAX)
    }
}
