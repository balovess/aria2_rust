//! Fresh (non-pooled) FTP connection negotiation helpers.
//!
//! Contains the `FtpNegotiator` impl methods for establishing a new FTP
//! control connection, authenticating, and querying server capabilities
//! (FEAT, OPTS UTF8 ON, SYST).

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, info};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::negotiation::FtpNegotiator;
use crate::ftp::connection::negotiation::capabilities;
use crate::ftp::connection::negotiation::capabilities::ServerCapabilities;
use crate::ftp::connection::negotiation::control::FreshControl;

impl FtpNegotiator {
    /// Connect to FTP server, read greeting, authenticate, and detect capabilities.
    ///
    /// After successful authentication, sends FEAT to detect server features
    /// and OPTS UTF8 ON if the server advertises UTF8 support (RFC 2640).
    /// This matches the C++ aria2 flow where FEAT/OPTS are sent right after
    /// login on every fresh connection.
    pub(super) async fn connect_and_authenticate(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        connect_timeout: Duration,
        command_timeout: Duration,
    ) -> Result<(FreshControl, ServerCapabilities)> {
        debug!("Connecting to FTP server at {}:{}", host, port);

        let stream = timeout(connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP connect failed to {}:{}: {}", host, port, e),
                })
            })?;

        stream.set_nodelay(true).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {}", e),
            })
        })?;

        let mut ctrl = FreshControl {
            reader: BufReader::new(stream),
            command_timeout,
        };

        // Read welcome message
        let welcome = ctrl.read_response(command_timeout).await?;
        if welcome.0 != 220 {
            // C++ aria2: EX_CONNECTION_FAILED -> FTP_PROTOCOL_ERROR for non-220 greeting
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!(
                        "FTP server rejected connection (expected 220): {} {}",
                        welcome.0, welcome.1
                    ),
                },
            ));
        }
        info!("Connected to FTP server {}:{}", host, port);

        // Authenticate using shared helper
        super::fresh_commands::authenticate(&mut ctrl, username, password).await?;

        // Query server capabilities via FEAT command
        let capabilities = capabilities::query_feat(&mut ctrl).await?;

        // If FEAT reports UTF8 support, send OPTS UTF8 ON (RFC 2640)
        if capabilities.utf8 {
            capabilities::send_opts_utf8_on(&mut ctrl).await?;
        }

        Ok((ctrl, capabilities))
    }
}
