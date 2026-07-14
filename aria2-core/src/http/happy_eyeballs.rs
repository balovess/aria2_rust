//! Happy Eyeballs (RFC 8305) implementation for low-latency dual-stack TCP connections.
//!
//! Races IPv6 and IPv4 connect attempts with a 250ms head start for IPv6.
//! When one address family is unreachable (e.g. IPv6 broken but IPv4 works),
//! fallback happens within ~250ms instead of waiting for a full SYN timeout
//! (~75s on Linux).
//!
//! The core race is implemented as a poll loop over `tokio::select!` with
//! precondition guards. All racing futures are stored as `BoxFuture` so they
//! are `Unpin` and can be polled by `&mut` reference across loop iterations
//! without being consumed.

use std::net::SocketAddr;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::net::TcpStream;
use tokio::time::sleep;
use tracing::debug;

/// Default IPv6 head start before attempting IPv4 (RFC 8305 recommends 250ms).
const DEFAULT_IPV6_HEAD_START: Duration = Duration::from_millis(250);

/// Connect to the first reachable address using Happy Eyeballs (RFC 8305).
///
/// Given a list of resolved socket addresses (mixed IPv4/IPv6), this function
/// races connection attempts with IPv6 getting a head start. The first
/// successful connection wins; the loser is cancelled.
///
/// # Arguments
/// * `addrs` - Resolved socket addresses (will be partitioned by address family)
///
/// # Returns
/// The first successfully connected `TcpStream`, or an error if all attempts
/// fail.
pub async fn connect_happy_eyeballs(addrs: Vec<SocketAddr>) -> std::io::Result<TcpStream> {
    connect_with_head_start(addrs, DEFAULT_IPV6_HEAD_START).await
}

/// Connect with a custom IPv6 head start duration (useful for testing).
///
/// When only one address family is present, the race is skipped and addresses
/// are tried sequentially. When both families are present, IPv6 is polled
/// immediately and IPv4 is held back until either `ipv6_head_start` elapses or
/// the IPv6 attempt fails, after which both races run concurrently until one
/// succeeds or both are exhausted.
pub async fn connect_with_head_start(
    addrs: Vec<SocketAddr>,
    ipv6_head_start: Duration,
) -> std::io::Result<TcpStream> {
    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no addresses to connect to",
        ));
    }

    // Partition addresses by family. Order within each family is preserved so
    // callers can pass preference-ordered address lists from DNS resolution.
    let mut ipv6_addrs: Vec<SocketAddr> = Vec::new();
    let mut ipv4_addrs: Vec<SocketAddr> = Vec::new();
    for addr in addrs {
        match addr {
            SocketAddr::V6(_) => ipv6_addrs.push(addr),
            SocketAddr::V4(_) => ipv4_addrs.push(addr),
        }
    }

    // Single-family fast path: skip the race entirely and connect sequentially.
    if ipv6_addrs.is_empty() {
        debug!("Happy Eyeballs: single-family (IPv4 only), skipping race");
        return connect_first_available(ipv4_addrs).await;
    }
    if ipv4_addrs.is_empty() {
        debug!("Happy Eyeballs: single-family (IPv6 only), skipping race");
        return connect_first_available(ipv6_addrs).await;
    }

    debug!(
        "Happy Eyeballs: racing {} IPv6 and {} IPv4 addresses (head start {:?})",
        ipv6_addrs.len(),
        ipv4_addrs.len(),
        ipv6_head_start,
    );

    // Box the racing futures so they are `Unpin` (trait objects are `Unpin`).
    // This lets `tokio::select!` poll them via `&mut` across loop iterations
    // without consuming them. `Pin<Box<Sleep>>` would be `!Unpin` (Sleep is
    // !Unpin) and therefore could NOT be polled by `&mut` in select!, so the
    // head-start timer is boxed as a `BoxFuture<'static, ()>` as well.
    // `FutureExt::boxed` yields `Pin<Box<dyn Future + Send>>` directly, which
    // is exactly `BoxFuture`.
    let mut ipv6_fut: BoxFuture<'static, std::io::Result<TcpStream>> =
        connect_first_available(ipv6_addrs).boxed();
    let mut ipv4_fut: BoxFuture<'static, std::io::Result<TcpStream>> =
        connect_first_available(ipv4_addrs).boxed();
    // Created ONCE here so the timer is not reset on every loop iteration.
    // (Recreating `sleep(...)` inside the select! macro each iteration would
    // restart the deadline and the head start would never elapse.)
    let mut head_start: BoxFuture<'static, ()> = sleep(ipv6_head_start).boxed();

    let mut ipv4_started = false;
    let mut ipv6_done = false;
    let mut ipv4_done = false;
    let mut ipv6_err: Option<std::io::Error> = None;
    let mut ipv4_err: Option<std::io::Error> = None;

    loop {
        tokio::select! {
            // Always poll IPv6 until it completes.
            result = &mut ipv6_fut, if !ipv6_done => {
                ipv6_done = true;
                match result {
                    Ok(stream) => {
                        debug!("Happy Eyeballs: IPv6 connected first");
                        return Ok(stream);
                    }
                    Err(e) => {
                        debug!("Happy Eyeballs: IPv6 attempt failed: {e}");
                        ipv6_err = Some(e);
                        // IPv6 failed — start IPv4 immediately, no need to wait
                        // for the head start to elapse.
                        ipv4_started = true;
                    }
                }
            }
            // Only poll IPv4 once it has been started (after head start or IPv6
            // failure).
            result = &mut ipv4_fut, if ipv4_started && !ipv4_done => {
                ipv4_done = true;
                match result {
                    Ok(stream) => {
                        debug!("Happy Eyeballs: IPv4 connected");
                        return Ok(stream);
                    }
                    Err(e) => {
                        debug!("Happy Eyeballs: IPv4 attempt failed: {e}");
                        ipv4_err = Some(e);
                    }
                }
            }
            // Head-start timer: only meaningful before IPv4 has started.
            _ = &mut head_start, if !ipv4_started => {
                ipv4_started = true;
                debug!("Happy Eyeballs: IPv6 head start elapsed, starting IPv4");
            }
        }

        // Both families exhausted — return the most informative error
        // available. Prefer the IPv4 error since IPv4 is the common fallback
        // path; fall back to the IPv6 error; otherwise a generic message.
        if ipv6_done && ipv4_done {
            return Err(ipv4_err
                .or(ipv6_err)
                .unwrap_or_else(|| std::io::Error::other("all connection attempts failed")));
        }
    }
}

/// Try connecting to addresses sequentially, returning the first success.
///
/// The last encountered error is preserved so callers can report a meaningful
/// failure reason when every address is unreachable.
async fn connect_first_available(addrs: Vec<SocketAddr>) -> std::io::Result<TcpStream> {
    let mut last_err = std::io::Error::other("no addresses");
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                debug!("Happy Eyeballs: connected to {addr}");
                return Ok(stream);
            }
            Err(e) => {
                debug!("Happy Eyeballs: connect to {addr} failed: {e}");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    #[tokio::test]
    async fn test_connect_empty_addresses_returns_error() {
        let result = connect_happy_eyeballs(vec![]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connect_ipv4_only_closed_port_fails() {
        // Connect to a closed port — should fail but not panic.
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1));
        let result = connect_happy_eyeballs(vec![addr]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connect_to_local_listener_succeeds() {
        // Start a local TCP listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_task = tokio::spawn(async move { connect_happy_eyeballs(vec![addr]).await });

        // Accept the connection so the connect can complete.
        let _conn = listener.accept().await.unwrap();
        let result = connect_task.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_happy_eyeballs_ipv6_unreachable_falls_back_to_ipv4() {
        // Start a local IPv4 listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let v4_addr = listener.local_addr().unwrap();

        // Closed port on IPv6 loopback. On most systems this fails fast with
        // ConnectionRefused (or NetworkUnreachable if IPv6 is disabled) rather
        // than hanging for a full SYN timeout.
        let v6_addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 1, 0, 0));

        let addrs = vec![v6_addr, v4_addr];
        let start = std::time::Instant::now();

        // Spawn the connect so the listener can accept concurrently. Awaiting
        // connect before accept would deadlock (connect needs accept to
        // proceed on the IPv4 address).
        let connect_task = tokio::spawn(async move {
            connect_with_head_start(addrs, Duration::from_millis(250)).await
        });

        let _conn = listener.accept().await.unwrap();
        let result = connect_task.await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "should connect via IPv4 fallback: {:?}",
            result.err()
        );
        // Should fall back within ~250ms + connect time, NOT a 75s SYN timeout.
        assert!(
            elapsed < Duration::from_secs(5),
            "fallback took too long: {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_connect_first_available_returns_first_success() {
        // Test the helper function directly.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_task = tokio::spawn(async move { connect_first_available(vec![addr]).await });

        let _conn = listener.accept().await.unwrap();
        let result = connect_task.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_connect_first_available_all_fail_returns_error() {
        // All closed ports — should return the last error.
        let addrs = vec![
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1)),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 2)),
        ];
        let result = connect_first_available(addrs).await;
        assert!(result.is_err());
    }
}
