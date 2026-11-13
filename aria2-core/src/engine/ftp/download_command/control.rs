//! Core integration for an FTP protocol control session.

use std::net::SocketAddr;
use std::time::Duration;

use aria2_protocol::ftp::connection::FtpActiveDataListener;
use tracing::{debug, info, warn};

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::active_data_bind_addr;

pub(super) use super::connection::RawFtpControl;
use super::connection::map_session_error;

impl RawFtpControl {
    pub(super) async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        info!("Authenticating as user: {}", username);
        self.reader
            .authenticate(username, password, command_timeout())
            .await
            .map_err(map_session_error)?;
        info!("FTP login successful");
        Ok(())
    }

    pub(super) async fn set_transfer_type(&mut self, transfer_type: &str) -> Result<()> {
        self.reader
            .set_transfer_type(transfer_type, command_timeout())
            .await
            .map_err(map_session_error)
    }

    pub(super) async fn prepare_remote_path(&mut self, remote_path: &str) -> Result<String> {
        self.reader
            .prepare_remote_path(remote_path, command_timeout())
            .await
            .map_err(map_session_error)
    }

    pub(super) async fn get_modification_time(
        &mut self,
        remote_path: &str,
    ) -> Result<Option<std::time::SystemTime>> {
        self.reader
            .modification_time(remote_path, command_timeout())
            .await
            .map_err(map_session_error)
    }

    pub(super) async fn set_resume_offset(&mut self, offset: u64) -> Result<bool> {
        let accepted = self
            .reader
            .set_restart_offset(offset, command_timeout())
            .await
            .map_err(map_session_error)?;
        if !accepted {
            warn!(
                offset,
                "FTP REST command not accepted; restarting from zero"
            );
        }
        Ok(accepted)
    }

    pub(super) async fn get_file_size(&mut self, remote_path: &str) -> Result<Option<u64>> {
        debug!(remote_path, "Querying FTP file size");
        self.reader
            .file_size(remote_path, command_timeout())
            .await
            .map_err(map_session_error)
    }

    /// Negotiate the FTP passive port, then connect through the core's
    /// outbound-address policy. PASV's advertised address is intentionally
    /// ignored; the control peer address is used for NAT and bounce protection.
    pub(super) async fn enter_passive_mode(&mut self) -> Result<tokio::net::TcpStream> {
        let timeout = command_timeout();
        let port = if self.connection.peer_addr.is_ipv6() {
            self.reader.passive_data_port(timeout).await
        } else {
            self.reader.pasv_data_port(timeout).await
        }
        .map_err(map_session_error)?;
        let data_addr = SocketAddr::new(self.connection.peer_addr.ip(), port);
        tokio::time::timeout(
            Duration::from_secs(constants::FTP_DATA_CONNECTION_TIMEOUT_SECS),
            self.outbound_network_policy.connect(data_addr),
        )
        .await
        .map_err(|_| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Data connection timeout via {data_addr}"),
            })
        })?
        .map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Data connection failed via {data_addr}: {error}"),
            })
        })
    }

    /// Bind the active listener to the control socket's local interface and
    /// delegate EPRT/PORT negotiation to the protocol session.
    pub(super) async fn enter_active_mode(&mut self) -> Result<FtpActiveDataListener> {
        let local_addr = self
            .reader
            .get_ref()
            .get_ref()
            .ok_or_else(|| Aria2Error::Network("FTP local address unavailable".into()))?
            .local_addr()
            .map_err(|error| {
                Aria2Error::Network(format!("FTP local address unavailable: {error}"))
            })?;
        let listener = tokio::net::TcpListener::bind(active_data_bind_addr(local_addr))
            .await
            .map_err(|error| {
                Aria2Error::Network(format!("FTP active listener bind failed: {error}"))
            })?;
        let listener_addr = listener.local_addr().map_err(|error| {
            Aria2Error::Network(format!("FTP active listener address unavailable: {error}"))
        })?;
        self.reader
            .advertise_active_endpoint(listener_addr, command_timeout())
            .await
            .map_err(map_session_error)?;
        Ok(FtpActiveDataListener::new(listener, listener_addr))
    }

    pub(super) async fn initiate_retr(&mut self, remote_path: &str) -> Result<()> {
        self.reader
            .retrieve(remote_path, command_timeout())
            .await
            .map_err(map_session_error)
    }

    pub(super) async fn read_transfer_complete(&mut self) -> Result<()> {
        match self
            .reader
            .read_response(Duration::from_secs(
                constants::FTP_TRANSFER_COMPLETE_TIMEOUT_SECS,
            ))
            .await
        {
            Ok(response) if response.code == 226 => {
                debug!(message = %response.message, "FTP transfer complete");
            }
            Ok(response) => {
                warn!(code = response.code, message = %response.message, "Unexpected FTP transfer completion response");
            }
            Err(error) => debug!(error = ?error, "FTP transfer completion response unavailable"),
        }
        Ok(())
    }

    pub(super) async fn abort_transfer(&mut self) {
        self.reader.abort(command_timeout()).await;
    }

    pub(super) async fn quit(mut self) -> Result<()> {
        self.reader.quit(command_timeout()).await;
        Ok(())
    }
}

fn command_timeout() -> Duration {
    Duration::from_secs(constants::FTP_COMMAND_TIMEOUT_SECS)
}
