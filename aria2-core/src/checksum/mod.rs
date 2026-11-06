#![allow(clippy::module_inception)]

pub mod check_integrity;
pub mod checksum;
pub mod chunk_checksum;
pub(crate) mod hash_worker_pool;
pub mod message_digest;

pub use checksum::verify_file;
