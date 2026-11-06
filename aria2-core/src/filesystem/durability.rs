//! File and containing-directory durability barriers used before persisting progress.

use crate::error::{Aria2Error, Result};
use std::path::{Path, PathBuf};

/// Create a directory tree and persist each newly created directory entry in
/// its immediate parent. Existing ancestors are left alone.
pub(crate) async fn create_directories(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    crate::filesystem::disk_io_pool::shared()
        .run(
            move || create_directories_sync(&path),
            "create durable output directories",
        )
        .await
}

fn create_directories_sync(path: &Path) -> Result<()> {
    let mut new_directories = Vec::<PathBuf>::new();
    let mut directory = path;
    while !directory.exists() {
        new_directories.push(directory.to_path_buf());
        let Some(parent) = directory.parent() else {
            break;
        };
        if parent.as_os_str().is_empty() || parent == directory {
            break;
        }
        directory = parent;
    }

    std::fs::create_dir_all(path).map_err(Aria2Error::from)?;
    // `new_directories` is leaf-first. Syncing each entry's parent in this
    // order persists nested directory names without requiring write access to
    // unrelated, pre-existing ancestors.
    for directory in new_directories {
        sync_directory_entry_sync(&directory)?;
    }
    Ok(())
}

/// Synchronize the directory entry containing `path`. Call after the file
/// itself has been synchronized.
pub(crate) async fn sync_parent_directories(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    crate::filesystem::disk_io_pool::shared()
        .run(
            move || sync_parent_directories_sync(&path),
            "sync output directory entries",
        )
        .await
}

/// Synchronize an existing payload and the directory entry that names it.
/// Use before creating a checkpoint from a resumed file prefix whose earlier
/// durability is not represented by a compatible checkpoint.
pub(crate) async fn sync_existing_payload(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    crate::filesystem::disk_io_pool::shared()
        .run(
            move || {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(Aria2Error::from)?;
                file.sync_data().map_err(Aria2Error::from)?;
                sync_parent_directories_sync(&path)
            },
            "sync existing resume payload",
        )
        .await
}

/// Persist an atomic sibling-file replacement and its directory entry.
pub(crate) async fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    crate::filesystem::disk_io_pool::shared()
        .run(
            move || replace_file_sync(&source, &destination),
            "durable atomic file replacement",
        )
        .await
}

/// Persist the removal of a file's directory entry.
pub(crate) async fn remove_file(path: &Path) -> Result<bool> {
    let path = path.to_path_buf();
    crate::filesystem::disk_io_pool::shared()
        .run(
            move || match std::fs::remove_file(&path) {
                Ok(()) => {
                    sync_parent_directories_sync(&path)?;
                    Ok(true)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(Aria2Error::from(error)),
            },
            "durable file removal",
        )
        .await
}

#[cfg(unix)]
pub(crate) fn sync_parent_directories_sync(path: &Path) -> Result<()> {
    let mut directories = Vec::<PathBuf>::new();
    let mut directory = parent_or_current(path);
    loop {
        directories.push(directory.to_path_buf());
        let Some(parent) = directory.parent() else {
            break;
        };
        if parent.as_os_str().is_empty() {
            if directory == Path::new(".") {
                break;
            }
            directory = Path::new(".");
            continue;
        }
        if parent == directory {
            break;
        }
        directory = parent;
    }

    // Start at the leaf so every directory entry is made durable after the
    // directory it names has already been synchronized.
    for directory in directories {
        std::fs::File::open(&directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                Aria2Error::Io(format!(
                    "failed to sync directory {}: {error}",
                    directory.display()
                ))
            })?;
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn sync_parent_directories_sync(path: &Path) -> Result<()> {
    sync_directory_entry_sync(path)
}

#[cfg(windows)]
fn sync_directory_entry_sync(path: &Path) -> Result<()> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let directory = parent_or_current(path);
    let wide_path = directory
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    // Directory handles require FILE_FLAG_BACKUP_SEMANTICS. FlushFileBuffers
    // requires write access; unsupported filesystems return an error so a
    // caller cannot publish a checkpoint without its namespace barrier.
    // SAFETY: `wide_path` is NUL-terminated and all pointers remain valid for
    // the duration of the synchronous CreateFileW call.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == -1isize as _ {
        return Err(Aria2Error::Io(format!(
            "failed to open directory {} for durable sync: {}",
            directory.display(),
            std::io::Error::last_os_error()
        )));
    }

    // SAFETY: `handle` is a valid owned file handle returned by CreateFileW;
    // File closes it after sync_all completes.
    let directory_file = unsafe { std::fs::File::from_raw_handle(handle) };
    directory_file.sync_all().map_err(|error| {
        Aria2Error::Io(format!(
            "failed to sync directory {}: {error}",
            directory.display()
        ))
    })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_parent_directories_sync(_path: &Path) -> Result<()> {
    Err(Aria2Error::Io(
        "durable directory synchronization is unsupported on this platform".into(),
    ))
}

#[cfg(unix)]
fn sync_directory_entry_sync(path: &Path) -> Result<()> {
    let directory = parent_or_current(path);
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            Aria2Error::Io(format!(
                "failed to sync directory {}: {error}",
                directory.display()
            ))
        })
}

#[cfg(not(any(unix, windows)))]
fn sync_directory_entry_sync(_path: &Path) -> Result<()> {
    Err(Aria2Error::Io(
        "durable directory synchronization is unsupported on this platform".into(),
    ))
}

#[cfg(unix)]
fn replace_file_sync(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination).map_err(Aria2Error::from)?;
    sync_parent_directories_sync(destination)
}

#[cfg(windows)]
fn replace_file_sync(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    // SAFETY: both paths are NUL-terminated UTF-16 buffers that remain alive
    // for the duration of the synchronous Win32 call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(Aria2Error::from(std::io::Error::last_os_error()));
    }
    // MOVEFILE_WRITE_THROUGH does not return until the rename is on disk, so
    // a second parent-directory flush here would repeat the same barrier.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn replace_file_sync(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination).map_err(Aria2Error::from)?;
    sync_parent_directories_sync(destination)
}

#[cfg(any(unix, windows))]
fn parent_or_current(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::{create_directories, remove_file, replace_file, sync_parent_directories};

    #[tokio::test]
    async fn durable_directory_creation_syncs_nested_entries() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("new").join("nested");

        create_directories(&nested).await.unwrap();

        assert!(nested.is_dir());
    }

    #[tokio::test]
    async fn durable_replace_replaces_the_destination_and_syncs_its_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("checkpoint.tmp");
        let destination = directory.path().join("checkpoint.aria2");
        tokio::fs::write(&source, b"new checkpoint").await.unwrap();
        tokio::fs::write(&destination, b"old checkpoint")
            .await
            .unwrap();

        replace_file(&source, &destination).await.unwrap();

        assert!(!source.exists());
        assert_eq!(
            tokio::fs::read(&destination).await.unwrap(),
            b"new checkpoint"
        );
    }

    #[tokio::test]
    async fn durable_remove_removes_the_entry_and_syncs_its_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.aria2");
        tokio::fs::write(&path, b"checkpoint").await.unwrap();

        remove_file(&path).await.unwrap();

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn parent_directory_sync_accepts_a_file_path() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("new").join("nested");
        tokio::fs::create_dir_all(&nested).await.unwrap();
        let path = nested.join("payload.bin");
        tokio::fs::write(&path, b"payload").await.unwrap();

        sync_parent_directories(&path).await.unwrap();
    }

    #[tokio::test]
    async fn existing_payload_sync_accepts_a_resumed_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resume.bin");
        tokio::fs::write(&path, b"resumed payload").await.unwrap();

        super::sync_existing_payload(&path).await.unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"resumed payload");
    }
}
