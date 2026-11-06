//! Platform-specific UDP multicast socket setup for the LPD receive loop.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

#[cfg(any(unix, windows))]
use tracing::debug;
#[cfg(unix)]
use tracing::warn;

use crate::constants;

/// Create and configure a UDP socket bound to the LPD multicast port.
///
/// Steps:
/// 1. Set SO_REUSEADDR (Unix) so multiple instances can bind
/// 2. Bind to the LPD port:
///    - Windows: bind to `0.0.0.0:6771` (port only) due to MinGW limitations
///    - Unix: bind to `239.192.152.143:6771` (multicast address + port)
/// 3. Join multicast group 239.192.152.143 on all interfaces
///
/// This mirrors C++ `LpdMessageReceiver::init()` which does:
/// ```cpp
/// #ifdef __MINGW32__
///     socket_->bindWithFamily(multicastPort_, AF_INET);
/// #else
///     socket_->bind(multicastAddress_.c_str(), multicastPort_, AF_INET);
/// #endif
///     socket_->joinMulticastGroup(multicastAddress_, multicastPort_, localAddr);
/// ```
#[cfg(test)]
pub(super) fn create_lpd_socket() -> Result<UdpSocket, String> {
    create_lpd_socket_with_config(constants::LPD_PORT, None)
}

pub(super) fn create_lpd_socket_with_config(
    listen_port: u16,
    interface: Option<Ipv4Addr>,
) -> Result<UdpSocket, String> {
    if listen_port == 0 {
        return Err("LPD listen port must be greater than zero".to_string());
    }

    let multicast_ip: Ipv4Addr = constants::LPD_MULTICAST_ADDRESS
        .parse()
        .map_err(|e| format!("Invalid LPD multicast IP: {}", e))?;

    let port = listen_port;
    let local_interface = interface.unwrap_or(Ipv4Addr::UNSPECIFIED);

    // On Unix, create socket -> set SO_REUSEADDR -> bind -> join group.
    // SO_REUSEADDR must be set BEFORE bind so multiple processes can
    // bind to the same multicast port. This mirrors the C++ behavior
    // where SocketCore sets SO_REUSEADDR during bind.
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;

        // Create an unbound UDP socket.
        // SAFETY: `libc::socket` is a standard POSIX syscall. AF_INET and
        // SOCK_DGRAM are valid constants. The protocol argument 0 lets the
        // kernel choose the default protocol for SOCK_DGRAM (UDP).
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(format!(
                "Failed to create LPD UDP socket: {}",
                std::io::Error::last_os_error()
            ));
        }

        // Set SO_REUSEADDR before binding.
        // SAFETY: `fd` is a valid open socket descriptor (checked above).
        // `optval` is a valid `i32` on the stack whose reference outlives the
        // call. `size_of::<i32>()` correctly describes the option value size.
        let optval: i32 = 1;
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const i32 as *const libc::c_void,
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if result != 0 {
            warn!(
                "Failed to set SO_REUSEADDR on LPD socket (non-fatal): {}",
                std::io::Error::last_os_error()
            );
        }

        let bind_addr = SocketAddrV4::new(multicast_ip, port);

        #[cfg(target_os = "macos")]
        let raw_bind_addr = libc::sockaddr_in {
            sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
            sin_family: libc::AF_INET as _,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_be_bytes(multicast_ip.octets()),
            },
            sin_zero: [0; 8],
        };
        #[cfg(not(target_os = "macos"))]
        let raw_bind_addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as _,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_be_bytes(multicast_ip.octets()),
            },
            sin_zero: [0; 8],
        };

        // SAFETY: `fd` is a valid socket descriptor, and `raw_bind_addr`
        // remains alive for the duration of the call. The descriptor is
        // closed explicitly on failure and transferred to `UdpSocket` on
        // success.
        let bind_result = unsafe {
            libc::bind(
                fd,
                (&raw_bind_addr as *const libc::sockaddr_in).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if bind_result != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(format!(
                "Failed to bind LPD socket to {}: {}",
                bind_addr, error
            ));
        }

        // SAFETY: `fd` is a valid open socket descriptor returned by a
        // successful `socket()` call above. `from_raw_fd` takes ownership
        // of the descriptor, and the socket will be closed when dropped.
        let socket = unsafe { UdpSocket::from_raw_fd(fd) };

        // Join multicast group on all interfaces.
        // C++: `socket_->joinMulticastGroup(multicastAddress_, multicastPort_, localAddr)`
        socket
            .join_multicast_v4(&multicast_ip, &local_interface)
            .map_err(|e| format!("Failed to join LPD multicast group: {}", e))?;

        debug!(local = ?socket.local_addr().ok(), "LPD receive socket created (Unix)");
        Ok(socket)
    }

    // On Windows, bind to the port only (not the multicast address).
    // C++: `socket_->bindWithFamily(multicastPort_, AF_INET)`
    // This is necessary because binding to the multicast address fails
    // under Windows/MinGW. Unlike Unix, we do not set SO_REUSEADDR
    // here because (a) the windows-sys crate does not expose WinSock,
    // and (b) Windows multicast sockets can share a bound port without
    // SO_REUSEADDR when joining the same multicast group.
    #[cfg(windows)]
    {
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);

        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| format!("Failed to bind LPD socket to {}: {}", bind_addr, e))?;

        // Join multicast group on all interfaces.
        socket
            .join_multicast_v4(&multicast_ip, &local_interface)
            .map_err(|e| format!("Failed to join LPD multicast group: {}", e))?;

        debug!(local = ?socket.local_addr().ok(), "LPD receive socket created (Windows)");
        Ok(socket)
    }

    // Fallback for other platforms (e.g., wasm). This is unlikely to
    // be used in practice but keeps the code compilable everywhere.
    #[cfg(not(any(unix, windows)))]
    {
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
        let socket =
            UdpSocket::bind(bind_addr).map_err(|e| format!("Failed to bind LPD socket: {}", e))?;
        socket
            .join_multicast_v4(&multicast_ip, &local_interface)
            .map_err(|e| format!("Failed to join LPD multicast group: {}", e))?;
        Ok(socket)
    }
}
