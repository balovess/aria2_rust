mod connection;
mod outbound_policy;

pub use connection::{ConnectionContext, EndpointKey};
pub use outbound_policy::OutboundNetworkPolicy;

pub(crate) async fn connect_from(
    local: std::net::IpAddr,
    remote: std::net::SocketAddr,
) -> std::io::Result<tokio::net::TcpStream> {
    let socket = match (local, remote) {
        (std::net::IpAddr::V4(_), std::net::SocketAddr::V4(_)) => tokio::net::TcpSocket::new_v4()?,
        (std::net::IpAddr::V6(_), std::net::SocketAddr::V6(_)) => tokio::net::TcpSocket::new_v6()?,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "local and remote address families differ",
            ));
        }
    };
    socket.bind(std::net::SocketAddr::new(local, 0))?;
    socket.connect(remote).await
}
