use std::path::Path;
use std::io::{Read, Write, Seek, SeekFrom};
use tracing::{debug, warn};

use super::session::SftpSession;

const DEFAULT_READ_BUF_SIZE: usize = 32 * 1024;
const DEFAULT_WRITE_BUF_SIZE: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub struct FileAttributes {
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub permissions: u32,
    pub atime: u64,
    pub mtime: u64,
    pub is_directory: bool,
    pub is_regular_file: bool,
    pub is_symlink: bool,
}

impl Default for FileAttributes {
    fn default() -> Self {
        Self {
            size: 0,
            uid: 0,
            gid: 0,
            permissions: 0o644,
            atime: 0,
            mtime: 0,
            is_directory: false,
            is_regular_file: false,
            is_symlink: false,
        }
    }
}

impl From<ssh2::FileStat> for FileAttributes {
    fn from(stat: ssh2::FileStat) -> Self {
        let is_dir = stat.file_type().is_dir();
        let is_file = stat.file_type().is_file();
        let is_link = stat.file_type().is_symlink();
        Self {
            size: stat.size.unwrap_or(0),
            uid: stat.uid.unwrap_or(0),
            gid: stat.gid.unwrap_or(0),
            permissions: stat.perm.unwrap_or(0),
            atime: stat.atime.unwrap_or(0),
            mtime: stat.mtime.unwrap_or(0),
            is_directory: is_dir,
            is_regular_file: is_file,
            is_symlink: is_link,
        }
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OpenFlags: u32 {
        const READ   = 0x0001;
        const WRITE  = 0x0002;
        const APPEND = 0x0004;
        const CREATE = 0x0008;
        const TRUNCATE = 0x0010;
        const CREATE_NEW = 0x0020;
    }
}

impl Default for OpenFlags {
    fn default() -> Self { Self::READ }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub attributes: FileAttributes,
}

pub struct SftpFileOps<'a> {
    session: &'a SftpSession,
}

impl<'a> SftpFileOps<'a> {
    pub fn new(session: &'a SftpSession) -> Self {
        Self { session }
    }

    pub fn session(&self) -> &'a SftpSession {
        self.session
    }

    pub fn stat(&self, path: &str) -> Result<FileAttributes, String> {
        debug!("STAT: {}", path);
        let stat = self.session.inner().stat(Path::new(path))
            .map_err(|e| format!("STAT failed [{}]: {}", path, e))?;
        Ok(FileAttributes::from(stat))
    }

    pub fn lstat(&self, path: &str) -> Result<FileAttributes, String> {
        debug!("LSTAT: {}", path);
        let stat = self.session.inner().lstat(Path::new(path))
            .map_err(|e| format!("LSTAT failed [{}]: {}", path, e))?;
        Ok(FileAttributes::from(stat))
    }

    pub fn set_stat(&self, path: &str, attrs: &FileAttributes) -> Result<(), String> {
        debug!("SETSTAT: {}", path);
        let ssh_stat = ssh2::FileStat {
            size: Some(attrs.size),
            uid: Some(attrs.uid as u32),
            gid: Some(attrs.gid as u32),
            perm: Some(attrs.permissions),
            atime: Some(attrs.atime),
            mtime: Some(attrs.mtime),
        };
        self.session.inner().setstat(Path::new(path), ssh_stat)
            .map_err(|e| format!("SETSTAT failed [{}]: {}", path, e))
    }

    pub fn realpath(&self, path: &str) -> Result<String, String> {
        debug!("REALPATH: {}", path);
        let resolved = self.session.inner().realpath(Path::new(path))
            .map_err(|e| format!("REALPATH failed [{}]: {}", path, e))?;
        Ok(resolved.to_string_lossy().to_string())
    }

    pub fn open(&self, path: &str, flags: OpenFlags, mode: u32) -> Result<SftpFileHandle<'a>, String> {
        debug!("OPEN: {} (mode={:o}, flags={:?})", path, mode, flags);
        let handle = if flags.contains(OpenFlags::WRITE) || flags.contains(OpenFlags::APPEND) || flags.contains(OpenFlags::CREATE) || flags.contains(OpenFlags::CREATE_NEW) {
            // Write operations - build ssh2 OpenFlags from our flags
            let mut ssh_flags = ssh2::OpenFlags::empty();
            if flags.contains(OpenFlags::READ) {
                ssh_flags |= ssh2::OpenFlags::READ;
            }
            if flags.contains(OpenFlags::WRITE) || flags.contains(OpenFlags::APPEND) || flags.contains(OpenFlags::CREATE) || flags.contains(OpenFlags::CREATE_NEW) {
                ssh_flags |= ssh2::OpenFlags::WRITE;
            }
            if flags.contains(OpenFlags::APPEND) {
                ssh_flags |= ssh2::OpenFlags::APPEND;
            }
            if flags.contains(OpenFlags::CREATE) || flags.contains(OpenFlags::CREATE_NEW) {
                ssh_flags |= ssh2::OpenFlags::CREATE;
            }
            self.session.inner().open_mode(
                Path::new(path),
                ssh_flags,
                mode as i32,
                ssh2::OpenType::File,
            )
                .map_err(|e| format!("OPEN(write) failed [{}]: {}", path, e))?
        } else {
            // Read-only
            self.session.inner().open(Path::new(path))
                .map_err(|e| format!("OPEN(read) failed [{}]: {}", path, e))?
        };
        Ok(SftpFileHandle { inner: handle, _marker: std::marker::PhantomData })
    }

    pub fn mkdir(&self, path: &str, mode: u32) -> Result<(), String> {
        debug!("MKDIR: {} (mode={:o})", path, mode);
        self.session.inner().mkdir(Path::new(path), mode as i32)
            .map_err(|e| format!("MKDIR failed [{}]: {}", path, e))
    }

    pub fn mkdir_recursive(&self, path: &str, mode: u32) -> Result<(), String> {
        let p = Path::new(path);
        if p.as_os_str().is_empty() {
            return Ok(());
        }
        if self.exists(path)? {
            let attr = self.lstat(path)?;
            if !attr.is_directory {
                return Err(format!("path exists but is not a directory: {}", path));
            }
            return Ok(());
        }
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                self.mkdir_recursive(parent.to_str().unwrap_or("."), mode)?;
            }
        }
        self.mkdir(path, mode)
    }

    pub fn rmdir(&self, path: &str) -> Result<(), String> {
        debug!("RMDIR: {}", path);
        self.session.inner().rmdir(Path::new(path))
            .map_err(|e| format!("RMDIR failed [{}]: {}", path, e))
    }

    pub fn unlink(&self, path: &str) -> Result<(), String> {
        debug!("UNLINK: {}", path);
        self.session.inner().unlink(Path::new(path))
            .map_err(|e| format!("UNLINK failed [{}]: {}", path, e))
    }

    pub fn rename(&self, src: &str, dest: &str) -> Result<(), String> {
        debug!("RENAME: {} -> {}", src, dest);
        self.session.inner().rename(Path::new(src), Path::new(dest), None)
            .map_err(|e| format!("RENAME failed [{} -> {}]: {}", src, dest, e))
    }

    pub fn symlink(&self, target: &str, link_path: &str) -> Result<(), String> {
        debug!("SYMLINK: {} -> {}", link_path, target);
        self.session.inner().symlink(Path::new(link_path), Path::new(target))
            .map_err(|e| format!("SYMLINK failed [{} -> {}]: {}", link_path, target, e))
    }

    pub fn readlink(&self, path: &str) -> Result<String, String> {
        debug!("READLINK: {}", path);
        let target = self.session.inner().readlink(Path::new(path))
            .map_err(|e| format!("READLINK failed [{}]: {}", path, e))?;
        Ok(target.to_string_lossy().to_string())
    }

    pub fn exists(&self, path: &str) -> Result<bool, String> {
        match self.lstat(path) {
            Ok(_) => Ok(true),
            Err(e) if e.contains("No such file") || e.contains("not found") => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, String> {
        debug!("READDIR: {}", path);
        let mut dir = self.session.inner().opendir(Path::new(path))
            .map_err(|e| format!("OPENDIR failed [{}]: {}", path, e))?;

        let mut entries = Vec::new();
        loop {
            match dir.readdir() {
                Ok((name, stat)) => {
                    let name_str = name.to_string_lossy().to_string();
                    if name_str == "." || name_str == ".." {
                        continue;
                    }
                    entries.push(DirEntry {
                        name: name_str,
                        attributes: FileAttributes::from(stat),
                    });
                }
                Err(e) if matches!(e.code(), ssh2::ErrorCode::Session(_)) && e.to_string().contains("end of file") => {
                    break;
                }
                Err(e) => {
                    warn!("READDIR error [{}]: {}", path, e);
                    break;
                }
            }
        }
        Ok(entries)
    }

    pub fn list_files(&self, path: &str) -> Result<Vec<String>, String> {
        let entries = self.readdir(path)?;
        let files: Vec<String> = entries.into_iter()
            .filter(|e| e.attributes.is_regular_file || e.attributes.is_symlink)
            .map(|e| e.name)
            .collect();
        Ok(files)
    }

    pub fn list_dirs(&self, path: &str) -> Result<Vec<String>, String> {
        let entries = self.readdir(path)?;
        let dirs: Vec<String> = entries.into_iter()
            .filter(|e| e.attributes.is_directory && e.name != "." && e.name != "..")
            .map(|e| e.name)
            .collect();
        Ok(dirs)
    }
}

pub struct SftpFileHandle<'a> {
    inner: ssh2::File,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> SftpFileHandle<'a> {
    pub fn fstat(&mut self) -> Result<FileAttributes, String> {
        let stat = self.inner.stat()
            .map_err(|e| format!("FSTAT failed: {}", e))?;
        Ok(FileAttributes::from(stat))
    }

    pub fn set_fstat(&mut self, attrs: &FileAttributes) -> Result<(), String> {
        let ssh_stat = ssh2::FileStat {
            size: Some(attrs.size),
            uid: Some(attrs.uid as u32),
            gid: Some(attrs.gid as u32),
            perm: Some(attrs.permissions),
            atime: Some(attrs.atime),
            mtime: Some(attrs.mtime),
        };
        self.inner.setstat(ssh_stat)
            .map_err(|e| format!("SETFSTAT failed: {}", e))
    }

    pub fn fsync(&mut self) -> Result<(), String> {
        self.inner.fsync()
            .map_err(|e| format!("FSYNC failed: {}", e))
    }

    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        self.inner.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seek to offset={} failed: {}", offset, e))?;
        let n = self.inner.read(buf)
            .map_err(|e| format!("READ failed at offset={}: {}", offset, e))?;
        Ok(n)
    }

    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<u64, String> {
        self.inner.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seek to offset={} failed: {}", offset, e))?;
        let n = self.inner.write(data)
            .map_err(|e| format!("WRITE failed at offset={}, len={}: {}", offset, data.len(), e))?;
        Ok(n as u64)
    }

    pub fn close(self) -> Result<(), String> {
        drop(self.inner);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_attributes_default() {
        let attrs = FileAttributes::default();
        assert_eq!(attrs.size, 0);
        assert!(!attrs.is_directory);
        assert!(!attrs.is_regular_file);
        assert!(!attrs.is_symlink);
        assert_eq!(attrs.permissions, 0o644);
    }

    #[test]
    fn test_open_flags_variants() {
        let _ = OpenFlags::READ;
        let _ = OpenFlags::WRITE;
        let _ = OpenFlags::READ | OpenFlags::WRITE;
        let _ = OpenFlags::APPEND;
        let _ = OpenFlags::CREATE;
        let _ = OpenFlags::CREATE_NEW;
        let _ = OpenFlags::TRUNCATE;
        // Test combination used by upload
        let combined = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
        assert!(combined.contains(OpenFlags::READ));
        assert!(combined.contains(OpenFlags::WRITE));
        assert!(combined.contains(OpenFlags::CREATE));
    }

    #[test]
    fn test_dir_entry_creation() {
        let entry = DirEntry {
            name: "test.txt".to_string(),
            attributes: FileAttributes {
                size: 1024,
                is_regular_file: true,
                ..Default::default()
            },
        };
        assert_eq!(entry.name, "test.txt");
        assert!(entry.attributes.is_regular_file);
        assert_eq!(entry.attributes.size, 1024);
    }

    #[test]
    fn test_buf_size_constants() {
        assert_eq!(DEFAULT_READ_BUF_SIZE, 32768);
        assert_eq!(DEFAULT_WRITE_BUF_SIZE, 32768);
    }
}
