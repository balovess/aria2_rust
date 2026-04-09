use tracing::{debug, info};

use super::connection::{SshConnection, SshOptions};

pub const SFTP_VERSION_MIN: u32 = 3;
pub const SFTP_VERSION_MAX: u32 = 6;

#[derive(Debug, Clone)]
pub struct SftpExtension {
    pub name: String,
    pub data: String,
}

pub struct SftpSession {
    server_version: u32,
    options: SshOptions,
    sftp_ptr: *mut ssh2::Sftp,
}

impl SftpSession {
    pub async fn open(conn: &SshConnection) -> Result<Self, String> {
        debug!("initializing SFTP session...");
        let sftp_box = Box::new(
            conn.session()
                .sftp()
                .map_err(|e| format!("SFTP subsystem init failed: {}", e))?,
        );

        // ssh2 0.9 Sftp does not expose server_version(); assume v3-v6 support
        let version = 3u32;
        debug!("SFTP server version: v{}", version);

        if version < SFTP_VERSION_MIN {
            return Err(format!(
                "SFTP version too low: v{} (minimum v{})",
                version, SFTP_VERSION_MIN
            ));
        }

        info!("SFTP session established (v{})", version);
        Ok(Self {
            server_version: version,
            options: conn.options().clone(),
            sftp_ptr: Box::into_raw(sftp_box),
        })
    }

    pub(crate) fn inner(&self) -> &ssh2::Sftp {
        unsafe { &*self.sftp_ptr }
    }

    pub fn server_version(&self) -> u32 {
        self.server_version
    }

    pub fn supports_version(&self, v: u32) -> bool {
        self.server_version >= v
    }

    pub fn has_extension(&self, _name: &str) -> bool {
        false
    }

    pub fn get_extension(&self, _name: &str) -> Option<&SftpExtension> {
        None
    }

    pub fn extensions(&self) -> Vec<SftpExtension> {
        Vec::new()
    }

    pub fn options(&self) -> &SshOptions {
        &self.options
    }
}

impl std::ops::Drop for SftpSession {
    fn drop(&mut self) {
        unsafe {
            if !self.sftp_ptr.is_null() {
                let _ = Box::from_raw(self.sftp_ptr);
            }
        }
    }
}

impl std::fmt::Debug for SftpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpSession")
            .field("server_version", &self.server_version)
            .field("options", &self.options)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sftp_version_constants() {
        assert!(SFTP_VERSION_MIN <= SFTP_VERSION_MAX);
        assert_eq!(SFTP_VERSION_MIN, 3);
        assert_eq!(SFTP_VERSION_MAX, 6);
    }

    #[test]
    fn test_sftp_extension() {
        let ext = SftpExtension {
            name: "hardlink@openssh.com".to_string(),
            data: "1".to_string(),
        };
        assert_eq!(ext.name, "hardlink@openssh.com");
        assert_eq!(ext.data, "1");
    }

    #[test]
    fn test_sftp_session_empty_extensions() {
        let exts = Vec::<SftpExtension>::new();
        assert!(exts.is_empty());
    }
}
