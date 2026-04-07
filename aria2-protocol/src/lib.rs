pub mod http;
pub mod ftp;

#[cfg(feature = "bittorrent")]
pub mod bittorrent;

#[cfg(feature = "metalink")]
pub mod metalink;

#[cfg(feature = "sftp")]
pub mod sftp;
