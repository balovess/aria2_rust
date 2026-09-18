use tokio::io::AsyncWriteExt;
use tokio::time::{Duration, interval};
use tracing::{debug, info, warn};

use super::FtpConnection;

impl FtpConnection {
    pub async fn login(&mut self) -> Result<(), String> {
        debug!("Sending USER command: {}", self.options.username);
        self.send_command(&format!("USER {}", self.options.username))
            .await?;
        let resp = self.read_response().await?;

        if resp.code == 331 || resp.code == 332 {
            debug!("Password required, sending PASS command");
            self.send_command(&format!("PASS {}", self.options.password))
                .await?;
            let pass_resp = self.read_response().await?;
            if !pass_resp.is_positive_completion() {
                return Err(format!(
                    "FTP login failed: {} {}",
                    pass_resp.code, pass_resp.message
                ));
            }
            info!("FTP login successful");
        } else if !resp.is_positive_completion() {
            return Err(format!("FTP login failed: {} {}", resp.code, resp.message));
        } else {
            info!("FTP login successful (no password required)");
        }

        Ok(())
    }

    pub async fn cwd(&mut self, path: &str) -> Result<(), String> {
        debug!("Changing directory: {}", path);
        self.send_command(&format!("CWD {}", path)).await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("CWD failed: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    pub async fn size(&mut self, filename: &str) -> Result<u64, String> {
        debug!("Querying file size: {}", filename);
        self.send_command(&format!("SIZE {}", filename)).await?;
        let resp = self.read_response().await?;
        if resp.code == 213 {
            let size_str = resp.message.trim();
            size_str
                .parse::<u64>()
                .map_err(|e| format!("Failed to parse file size: {} ({})", e, size_str))
        } else {
            Err(format!(
                "SIZE command failed: {} {}",
                resp.code, resp.message
            ))
        }
    }

    pub async fn mdtm(&mut self, filename: &str) -> Result<String, String> {
        debug!("Querying modification time: {}", filename);
        self.send_command(&format!("MDTM {}", filename)).await?;
        let resp = self.read_response().await?;
        if resp.code == 213 {
            Ok(resp.message.trim().to_string())
        } else {
            Err(format!(
                "MDTM command failed: {} {}",
                resp.code, resp.message
            ))
        }
    }

    pub async fn rest(&mut self, offset: u64) -> Result<(), String> {
        debug!("Setting resume offset: {}", offset);
        self.send_command(&format!("REST {}", offset)).await?;
        let resp = self.read_response().await?;
        if resp.code != 350 {
            return Err(format!("REST failed: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    pub async fn retr(&mut self, filename: &str) -> Result<(), String> {
        debug!("Preparing to download file: {}", filename);
        self.send_command(&format!("RETR {}", filename)).await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_preliminary() {
            return Err(format!("RETR failed: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    pub async fn type_image(&mut self) -> Result<(), String> {
        debug!("Setting transfer type to binary (I)");
        self.send_command("TYPE I").await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("TYPE I failed: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    pub async fn quit(mut self) -> Result<(), String> {
        debug!("Sending QUIT command");
        self.send_command("QUIT").await?;
        let resp = self.read_response().await?;
        info!("FTP disconnected: {}", resp.message);
        Ok(())
    }

    /// Send ABOR command to abort a transfer in progress.
    pub async fn abor(&mut self) -> Result<(), String> {
        debug!("Sending ABOR command to abort transfer");
        self.stream
            .get_mut()
            .write_all(b"\xff\xf4")
            .await
            .map_err(|e| format!("Failed to send Telnet IP: {}", e))?;
        self.stream
            .get_mut()
            .flush()
            .await
            .map_err(|e| format!("Failed to flush buffer: {}", e))?;

        tokio::time::sleep(Duration::from_millis(100)).await;

        self.send_command("ABOR").await?;
        let resp = self.read_response().await?;
        if resp.code == 226 || resp.code == 225 {
            debug!("Transfer successfully aborted: {}", resp.message);
            Ok(())
        } else {
            warn!("ABOR response unusual: {} {}", resp.code, resp.message);
            Ok(())
        }
    }

    /// Send TYPE A command for ASCII mode transfer.
    pub async fn type_ascii(&mut self) -> Result<(), String> {
        debug!("Setting transfer type to ASCII (A)");
        self.send_command("TYPE A").await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("TYPE A failed: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    /// Send NOOP command for keep-alive / connection test.
    pub async fn noop(&mut self) -> Result<(), String> {
        debug!("Sending NOOP keep-alive command");
        self.send_command("NOOP").await?;
        let resp = self.read_response().await?;
        if resp.code == 200 {
            debug!("NOOP keep-alive successful");
            Ok(())
        } else {
            Err(format!("NOOP failed: {} {}", resp.code, resp.message))
        }
    }

    /// Start a placeholder keep-alive task.
    ///
    /// The task currently emits a tick only; it does not own the control
    /// stream and therefore cannot send `NOOP` safely.
    pub fn start_keepalive(&self) -> Option<tokio::task::JoinHandle<()>> {
        let keepalive_duration = self.options.keepalive_interval?;

        Some(tokio::spawn(async move {
            let mut ticker = interval(keepalive_duration);
            loop {
                ticker.tick().await;
                debug!("FTP keep-alive tick");
            }
        }))
    }
}
