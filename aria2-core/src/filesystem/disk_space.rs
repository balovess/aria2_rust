use crate::error::{Aria2Error, FatalError, Result};
use std::path::Path;

const DEFAULT_MARGIN_MB: u64 = 100;

pub fn available_space(path: &Path) -> Result<u64> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };

    #[cfg(target_family = "unix")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::statvfs(path.as_os_str().as_bytes().as_ptr() as *const i8, &mut stat)
        };
        if ret != 0 {
            return Err(Aria2Error::Fatal(FatalError::Config(
                format!("statvfs failed: {}", std::io::Error::last_os_error())
            )));
        }
        Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
    }

    #[cfg(target_family = "windows")]
    {
        std::fs::metadata(path)
            .map(|_| {
                let _ = path;
                u64::MAX / 2
            })
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Failed to get disk space: {}", e))))
    }

    #[cfg(all(not(target_family = "unix"), not(target_family = "windows")))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}

pub fn has_enough_space(path: &Path, required: u64) -> bool {
    available_space(path).map_or(false, |avail| avail >= required)
}

pub fn check_with_margin(path: &Path, required: u64, margin_mb: Option<u64>) -> Result<()> {
    let margin = margin_mb.unwrap_or(DEFAULT_MARGIN_MB) * 1024 * 1024;
    let total_needed = required.saturating_add(margin);
    let avail = available_space(path)?;
    if avail < total_needed {
        Err(Aria2Error::Fatal(FatalError::DiskSpaceExhausted))
    } else {
        Ok(())
    }
}

pub fn total_space(path: &Path) -> Result<u64> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };

    #[cfg(target_family = "unix")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::statvfs(path.as_os_str().as_bytes().as_ptr() as *const i8, &mut stat)
        };
        if ret != 0 {
            return Err(Aria2Error::Fatal(FatalError::Config(
                format!("statvfs failed: {}", std::io::Error::last_os_error())
            )));
        }
        Ok(stat.f_blocks as u64 * stat.f_frsize as u64)
    }

    #[cfg(target_family = "windows")]
    {
        std::fs::metadata(path)
            .map(|_| u64::MAX)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Failed to get disk space: {}", e))))
    }

    #[cfg(all(not(target_family = "unix"), not(target_family = "windows")))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}
