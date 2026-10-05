use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::node::DhtNode;
use tokio::io::AsyncWriteExt;

/// Cross-process lock around a snapshot file write.
///
/// The lock file is intentionally retained on disk; the OS lock is tied to
/// the open handle and is released automatically after a crash or normal
/// drop, so stale marker cleanup is never required.
struct ProcessFileLock {
    _file: std::fs::File,
}

fn persistence_lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn ensure_parent_directory_sync(path: &Path) -> Result<(), String> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };

    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Failed to create DHT directory {}: {}",
            parent.display(),
            error
        )
    })
}

fn acquire_process_file_lock(path: &Path) -> Result<ProcessFileLock, String> {
    ensure_parent_directory_sync(path)?;
    let lock_path = persistence_lock_path(path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| {
            format!(
                "Failed to open DHT lock file {}: {}",
                lock_path.display(),
                e
            )
        })?;

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(format!(
                "Failed to lock DHT file {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        let result = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if result == 0 {
            return Err(format!(
                "Failed to lock DHT file {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }

    Ok(ProcessFileLock { _file: file })
}

const DHT_MAGIC: &[u8] = &[0xA1, 0xA2];
const DHT_FORMAT_ID: u8 = 0x02;
const DHT_VERSION_2: u8 = 0x02;
const DHT_VERSION_3: u8 = 0x03;
const NODE_ENTRY_SIZE: usize = 56;

#[derive(Debug, Clone)]
pub struct PersistedNode {
    pub id: [u8; 20],
    pub addr: std::net::SocketAddr,
}

#[derive(Debug, Clone)]
pub struct DhtPersistedData {
    pub self_id: [u8; 20],
    pub saved_at_secs: u64,
    pub nodes: Vec<PersistedNode>,
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn socket_addr_to_compact(addr: &std::net::SocketAddr) -> Vec<u8> {
    match addr {
        std::net::SocketAddr::V4(v4) => {
            let mut buf = vec![0u8; 6];
            buf[..4].copy_from_slice(&v4.ip().octets());
            buf[4..6].copy_from_slice(&v4.port().to_be_bytes());
            buf
        }
        std::net::SocketAddr::V6(v6) => {
            let mut buf = vec![0u8; 18];
            buf[..16].copy_from_slice(&v6.ip().octets());
            buf[16..18].copy_from_slice(&v6.port().to_be_bytes());
            buf
        }
    }
}

fn compact_to_socket_addr(data: &[u8]) -> Option<std::net::SocketAddr> {
    if data.len() == 6 {
        let ip = std::net::Ipv4Addr::new(data[0], data[1], data[2], data[3]);
        let port = u16::from_be_bytes([data[4], data[5]]);
        Some(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
            ip, port,
        )))
    } else if data.len() == 18 {
        let octets: [u8; 16] = data[..16].try_into().ok()?;
        let ip = std::net::Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([data[16], data[17]]);
        Some(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
            ip, port, 0, 0,
        )))
    } else {
        None
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("Failed to sync DHT directory {}: {}", parent.display(), e))?;
    }

    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

async fn sync_parent_directory_async(path: &Path) -> Result<(), String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || sync_parent_directory(&path))
        .await
        .map_err(|e| format!("Failed to sync DHT directory: {}", e))??;
    Ok(())
}

async fn write_serialized_to_file_async(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp_path = path.with_extension(format!("dat.tmp{}", rand::random::<u32>()));
    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| format!("Failed to write temp file {}: {}", tmp_path.display(), e))?;
    if let Err(error) = file.write_all(data).await {
        drop(file);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(format!(
            "Failed to write temp file {}: {}",
            tmp_path.display(),
            error
        ));
    }
    if let Err(error) = file.sync_all().await {
        drop(file);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(format!(
            "Failed to sync temp file {}: {}",
            tmp_path.display(),
            error
        ));
    }
    drop(file);

    let replacement_path = tmp_path.clone();
    let target_path = path.to_path_buf();
    let replacement =
        tokio::task::spawn_blocking(move || replace_file(&replacement_path, &target_path))
            .await
            .map_err(|e| format!("Failed to replace DHT file: {}", e))?;
    if let Err(error) = replacement {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(format!(
            "Failed to replace {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            error
        ));
    }

    sync_parent_directory_async(path).await
}

fn write_serialized_to_file_sync(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp_path = path.with_extension(format!("dat.tmp{}", rand::random::<u32>()));
    let mut file = std::fs::File::create(&tmp_path)
        .map_err(|e| format!("Failed to write temp file {}: {}", tmp_path.display(), e))?;
    if let Err(error) = file.write_all(data) {
        drop(file);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "Failed to write temp file {}: {}",
            tmp_path.display(),
            error
        ));
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "Failed to sync temp file {}: {}",
            tmp_path.display(),
            error
        ));
    }
    drop(file);

    if let Err(error) = replace_file(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "Failed to replace {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            error
        ));
    }

    sync_parent_directory(path)
}

fn replace_file(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

        let source: Vec<u16> = temp_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let target: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(temp_path, path)
    }
}

pub struct DhtPersistence;

impl DhtPersistence {
    /// Return whether a snapshot is recent enough to seed a routing table.
    pub fn is_fresh(saved_at_secs: u64, max_age: std::time::Duration) -> bool {
        current_epoch_secs().saturating_sub(saved_at_secs) <= max_age.as_secs()
    }
    pub fn serialize(self_id: &[u8; 20], nodes: &[DhtNode]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(56 + nodes.len() * NODE_ENTRY_SIZE);

        let mut header = [0u8; 8];
        header[0] = DHT_MAGIC[0];
        header[1] = DHT_MAGIC[1];
        header[2] = DHT_FORMAT_ID;
        header[6] = 0;
        header[7] = DHT_VERSION_3;
        buf.extend_from_slice(&header);

        let timestamp = current_epoch_secs().to_be_bytes();
        buf.extend_from_slice(&timestamp);

        let reserved8 = [0u8; 8];
        buf.extend_from_slice(&reserved8);
        buf.extend_from_slice(self_id);
        let reserved4 = [0u8; 4];
        buf.extend_from_slice(&reserved4);

        let node_count = (nodes.len() as u32).to_be_bytes();
        buf.extend_from_slice(&node_count);
        buf.extend_from_slice(&reserved4);

        for node in nodes {
            let compact = socket_addr_to_compact(&node.addr);
            let clen = compact.len() as u8;

            buf.push(clen);
            let reserved7 = [0u8; 7];
            buf.extend_from_slice(&reserved7);
            buf.extend_from_slice(&compact);

            let pad_len = 24 - compact.len();
            let padding = vec![0u8; pad_len];
            buf.extend_from_slice(&padding);

            buf.extend_from_slice(&node.id);
            buf.extend_from_slice(&reserved4);
        }

        buf
    }

    pub fn deserialize(data: &[u8]) -> Result<DhtPersistedData, String> {
        if data.len() < 56 {
            return Err("dht.dat data too short".into());
        }

        let header = |version| {
            [
                DHT_MAGIC[0],
                DHT_MAGIC[1],
                DHT_FORMAT_ID,
                0,
                0,
                0,
                0,
                version,
            ]
        };
        let saved_at_secs = if data[..8] == header(DHT_VERSION_3) {
            u64::from_be_bytes(
                data[8..16]
                    .try_into()
                    .map_err(|_| "dht.dat timestamp truncated")?,
            )
        } else if data[..8] == header(DHT_VERSION_2) {
            u32::from_be_bytes(
                data[8..12]
                    .try_into()
                    .map_err(|_| "dht.dat timestamp truncated")?,
            ) as u64
        } else {
            return Err(format!(
                "dht.dat invalid magic/version: {:02x?}",
                &data[..8]
            ));
        };

        // Both supported versions place the local-node record after a
        // 8-byte timestamp slot: v2 stores a 32-bit timestamp plus 4 reserved
        // bytes, while v3 stores a 64-bit timestamp.
        let mut offset = 16;

        if offset + 32 > data.len() {
            return Err("dht.dat localnode truncated".into());
        }
        offset += 8;
        let self_id: [u8; 20] = data[offset..offset + 20]
            .try_into()
            .map_err(|_| "dht.dat self_id length error")?;
        offset += 20;
        offset += 4;

        if offset + 8 > data.len() {
            return Err("dht.dat node count truncated".into());
        }
        let num_nodes = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 8;

        let available = data.len() - offset;
        if num_nodes > available / NODE_ENTRY_SIZE {
            let expected_end = offset.saturating_add(num_nodes.saturating_mul(NODE_ENTRY_SIZE));
            return Err(format!(
                "dht.dat node data truncated: need {} bytes, got {}",
                expected_end,
                data.len()
            ));
        }

        let mut nodes = Vec::with_capacity(num_nodes);
        for _ in 0..num_nodes {
            let entry_end = offset + NODE_ENTRY_SIZE;
            let clen = data[offset] as usize;

            if clen != 6 && clen != 18 {
                return Err(format!(
                    "dht.dat invalid compact peer info length: {}",
                    clen
                ));
            }

            let compact_start = offset + 8;
            let compact = &data[compact_start..compact_start + clen];
            let addr = compact_to_socket_addr(compact)
                .ok_or_else(|| "dht.dat compact peer info is malformed".to_string())?;

            let id_start = offset + 8 + 24;
            let id: [u8; 20] = data[id_start..id_start + 20]
                .try_into()
                .map_err(|_| "dht.dat node ID length error")?;

            nodes.push(PersistedNode { id, addr });
            offset = entry_end;
        }

        Ok(DhtPersistedData {
            self_id,
            saved_at_secs,
            nodes,
        })
    }

    pub async fn save_to_file(
        path: &Path,
        self_id: &[u8; 20],
        nodes: &[DhtNode],
    ) -> Result<usize, String> {
        let data = Self::serialize(self_id, nodes);

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                format!("Failed to create DHT directory {}: {}", parent.display(), e)
            })?;
        }

        let lock_path = path.to_path_buf();
        let _process_lock =
            tokio::task::spawn_blocking(move || acquire_process_file_lock(&lock_path))
                .await
                .map_err(|e| format!("Failed to acquire DHT file lock: {}", e))??;
        write_serialized_to_file_async(path, &data).await?;

        Ok(nodes.len())
    }

    pub fn save_to_file_sync(
        path: &Path,
        self_id: &[u8; 20],
        nodes: &[DhtNode],
    ) -> Result<usize, String> {
        let data = Self::serialize(self_id, nodes);
        let _process_lock = acquire_process_file_lock(path)?;
        write_serialized_to_file_sync(path, &data)?;

        Ok(nodes.len())
    }

    pub async fn load_from_file(path: &Path) -> Result<DhtPersistedData, String> {
        let data = tokio::fs::read(path)
            .await
            .map_err(|e| format!("Failed to read dht.dat {}: {}", path.display(), e))?;
        Self::deserialize(&data)
    }

    pub fn load_from_file_sync(path: &Path) -> Result<DhtPersistedData, String> {
        let data = std::fs::read(path)
            .map_err(|e| format!("Failed to read dht.dat {}: {}", path.display(), e))?;
        Self::deserialize(&data)
    }
}

#[cfg(test)]
mod tests;
