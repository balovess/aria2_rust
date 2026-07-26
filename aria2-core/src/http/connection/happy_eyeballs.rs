//! Happy Eyeballs (RFC 6555/8305) dual-stack connection racing.
//!
//! When the primary connection target is an IPv6 address, we race a backup
//! IPv4 connection after a configurable stagger delay (default 300ms, matching
//! C++ aria2). Whichever connects first wins; the other is cancelled.
//!
//! This mirrors C++ `BackupIPv4ConnectCommand` + `BackupConnectInfo`.
//!
//! # Algorithm
//!
//! 1. If primary is IPv6 and a backup IPv4 is available, start both.
//! 2. The primary connection begins immediately.
//! 3. After `stagger_delay` (300ms by default), the backup IPv4 connection starts.
//! 4. If the primary connects before the stagger expires, it wins outright.
//! 5. If the stagger expires before the primary connects, both race in parallel.
//! 6. Whichever completes first wins; the other is implicitly dropped.
//!
//! # Why not `tokio::net::TcpStream::connect(addr_iter)`?
//!
//! Tokio's built-in multi-address connect does sequential attempts with its own
//! internal stagger. We need explicit control over the stagger delay and the
//! ability to report *which* address won (for metrics and fallback decisions).
//!
//! # Implementation note
//!
//! Uses `std::pin::pin!` + `&mut` references so that futures are never moved.
//! This allows `tokio::select!` to poll them by reference, and the non-winning
//! future remains available for `.await` in the winning branch's error path.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time;
use tracing::{debug, trace};

/// Default stagger delay before starting the backup connection (300ms).
/// Matches C++ aria2's `BackupIPv4ConnectCommand` delay.
pub const DEFAULT_STAGGER_DELAY: Duration = Duration::from_millis(300);

/// Result of a Happy Eyeballs connection race.
#[derive(Debug)]
pub struct HappyEyeballsResult {
    /// The winning TCP stream.
    pub stream: TcpStream,
    /// The address that won the race.
    pub winning_addr: SocketAddr,
    /// Whether the backup (IPv4) connection won.
    pub backup_won: bool,
}

/// Attempt a dual-stack connection race.
///
/// If `primary_addr` is IPv6 and `backup_ipv4` is provided, race them with
/// a stagger delay. If `primary_addr` is IPv4 or no backup is available,
/// just connect to primary directly (no race needed).
///
/// # Arguments
///
/// * `primary_addr` - The first address to try (typically IPv6 from DNS)
/// * `backup_ipv4` - An optional IPv4 fallback address for the race
/// * `stagger_delay` - How long to wait before starting the backup attempt
/// * `connect_timeout` - Overall connection timeout
///
/// # Errors
///
/// Returns `io::Error` if both primary and backup connections fail, or if
/// the overall `connect_timeout` is exceeded.
pub async fn connect_with_happy_eyeballs(
    primary_addr: SocketAddr,
    backup_ipv4: Option<SocketAddr>,
    stagger_delay: Duration,
    connect_timeout: Duration,
) -> std::io::Result<HappyEyeballsResult> {
    // If no backup or primary is already IPv4, just connect directly.
    let backup = match (backup_ipv4, primary_addr) {
        (Some(v4), SocketAddr::V6(_)) => v4,
        _ => {
            let stream = time::timeout(connect_timeout, TcpStream::connect(primary_addr))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "Connection timed out")
                })??;
            return Ok(HappyEyeballsResult {
                stream,
                winning_addr: primary_addr,
                backup_won: false,
            });
        }
    };

    // Race: start primary immediately, backup after stagger delay.
    debug!(
        primary = %primary_addr,
        backup = %backup,
        "Starting Happy Eyeballs dual-stack race"
    );

    let result = time::timeout(
        connect_timeout,
        race_primary_and_backup(primary_addr, backup, stagger_delay),
    )
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Happy Eyeballs connection timed out",
        )
    })??;

    Ok(result)
}

/// Core racing logic, separated for clarity and testability.
///
/// Phase 1: Start primary immediately. If it completes before the stagger
/// delay, we are done.
///
/// Phase 2: If the stagger expires and primary hasn't finished yet, start
/// the backup too and race both in parallel.
///
/// Uses `std::pin::pin!` to pin the primary future on the stack so it can
/// be polled by reference across both phases without being consumed. This
/// ensures the primary connection attempt is never lost or restarted.
async fn race_primary_and_backup(
    primary_addr: SocketAddr,
    backup: SocketAddr,
    stagger_delay: Duration,
) -> std::io::Result<HappyEyeballsResult> {
    let mut primary_fut = std::pin::pin!(TcpStream::connect(primary_addr));

    tokio::select! {
        // Phase 1: Primary connected before stagger expired.
        res = &mut primary_fut => {
            match res {
                Ok(stream) => {
                    debug!(addr = %primary_addr, "Primary IPv6 won before stagger");
                    Ok(HappyEyeballsResult {
                        stream,
                        winning_addr: primary_addr,
                        backup_won: false,
                    })
                }
                Err(e) => {
                    // Primary failed quickly, try backup immediately.
                    trace!(error = %e, "Primary IPv6 failed, trying backup IPv4");
                    match TcpStream::connect(backup).await {
                        Ok(stream) => {
                            debug!(addr = %backup,
                                "Backup IPv4 succeeded after primary failure");
                            Ok(HappyEyeballsResult {
                                stream,
                                winning_addr: backup,
                                backup_won: true,
                            })
                        }
                        Err(_) => Err(e), // Return the primary error
                    }
                }
            }
        }
        // Phase 2: Stagger expired, primary still in progress.
        // Start the backup and race both. The primary future is NOT consumed
        // here — it was polled by reference (`&mut primary_fut`), so we can
        // continue polling it alongside the new backup future.
        _ = time::sleep(stagger_delay) => {
            debug!("Stagger expired, starting backup IPv4 in parallel with primary");
            let mut backup_fut = std::pin::pin!(TcpStream::connect(backup));

            // Race primary vs backup. `biased;` ensures we prefer the primary
            // when both are ready simultaneously (IPv6-first policy per RFC 8305).
            tokio::select! {
                biased;

                res = &mut primary_fut => {
                    match res {
                        Ok(stream) => {
                            debug!(addr = %primary_addr,
                                "Primary IPv6 won after stagger");
                            Ok(HappyEyeballsResult {
                                stream,
                                winning_addr: primary_addr,
                                backup_won: false,
                            })
                        }
                        Err(e) => {
                            // Primary failed, wait for backup.
                            trace!(error = %e,
                                "Primary IPv6 failed in race, waiting for backup");
                            match (&mut backup_fut).await {
                                Ok(stream) => {
                                    debug!(addr = %backup,
                                        "Backup IPv4 won after primary failed");
                                    Ok(HappyEyeballsResult {
                                        stream,
                                        winning_addr: backup,
                                        backup_won: true,
                                    })
                                }
                                Err(_) => Err(e),
                            }
                        }
                    }
                }

                res = &mut backup_fut => {
                    match res {
                        Ok(stream) => {
                            debug!(addr = %backup, "Backup IPv4 won the race");
                            Ok(HappyEyeballsResult {
                                stream,
                                winning_addr: backup,
                                backup_won: true,
                            })
                        }
                        Err(e) => {
                            // Backup failed, wait for primary.
                            trace!(error = %e,
                                "Backup IPv4 failed in race, waiting for primary");
                            match (&mut primary_fut).await {
                                Ok(stream) => {
                                    debug!(addr = %primary_addr,
                                        "Primary IPv6 won after backup failed");
                                    Ok(HappyEyeballsResult {
                                        stream,
                                        winning_addr: primary_addr,
                                        backup_won: false,
                                    })
                                }
                                Err(_) => Err(e),
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Resolve a hostname to both IPv6 and IPv4 addresses for Happy Eyeballs.
///
/// Returns `(primary, backup_ipv4)` where:
/// - `primary` is the first IPv6 address if available, otherwise the first IPv4
/// - `backup_ipv4` is `Some(first_ipv4)` when an IPv6 primary was chosen
///
/// This ordering matches RFC 6555 section 4: prefer IPv6 first, with IPv4
/// as the racing fallback.
///
/// # Arguments
///
/// * `host` - Hostname to resolve (must not include port)
/// * `port` - Port number to append to each resolved address
///
/// # Errors
///
/// Returns `io::Error` with `AddrNotAvailable` if no addresses are found.
pub async fn resolve_dual_stack(
    host: &str,
    port: u16,
) -> std::io::Result<(SocketAddr, Option<SocketAddr>)> {
    use tokio::net::lookup_host;

    let addr_str = format!("{}:{}", host, port);
    let addrs: Vec<SocketAddr> = lookup_host(&addr_str).await?.collect();

    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("No addresses found for {}", host),
        ));
    }

    let ipv6 = addrs.iter().find(|a| a.is_ipv6());
    let ipv4 = addrs.iter().find(|a| a.is_ipv4());

    match (ipv6, ipv4) {
        (Some(v6), Some(v4)) => Ok((*v6, Some(*v4))),
        (None, Some(v4)) => Ok((*v4, None)),
        (Some(v6), None) => Ok((*v6, None)),
        (None, None) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "No usable addresses found",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stagger_delay_is_300ms() {
        assert_eq!(DEFAULT_STAGGER_DELAY, Duration::from_millis(300));
    }

    #[tokio::test]
    async fn ipv4_primary_no_race() {
        // When primary is IPv4, backup should be ignored.
        let primary: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let backup: SocketAddr = "127.0.0.1:2".parse().unwrap();

        // This will fail to connect (port 1), but the important thing is
        // that it tries only the primary, not the backup.
        let result = connect_with_happy_eyeballs(
            primary,
            Some(backup),
            DEFAULT_STAGGER_DELAY,
            Duration::from_secs(1),
        )
        .await;

        // Should fail (connection refused), but the attempt was made to primary only.
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn resolve_dual_stack_localhost() {
        // localhost should resolve to at least one address.
        let result = resolve_dual_stack("localhost", 80).await;
        // May succeed or fail depending on the system's hosts file.
        // On most systems, localhost resolves to 127.0.0.1.
        if let Ok((primary, backup)) = result {
            // If primary is IPv4, backup should be None (no racing needed).
            if primary.is_ipv4() {
                assert!(backup.is_none());
            }
            // If primary is IPv6, backup may or may not be present.
        }
    }
}
