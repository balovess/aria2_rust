//! Hash verification state for piece completion tracking.
//!
//! Provides `HashState` (static-dispatch enum over supported hash algorithms)
//! and `finalize_hash` for computing piece digests. Uses an enum instead of
//! dynamic dispatch (`Box<dyn DynDigest>`) for zero-overhead algorithm selection.

use digest::Digest;
use tracing::trace;

/// Supported hash algorithms for piece verification.
///
/// Uses static dispatch (enum) instead of dynamic dispatch (`Box<dyn DynDigest>`)
/// for zero-overhead algorithm selection and no `alloc` feature dependency.
pub(crate) enum HashState {
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Md5(md5::Md5),
}

impl HashState {
    /// Creates a new hash state from a hash type name.
    ///
    /// Supports common names: "sha-1", "sha1", "sha-256", "sha256", "sha-512",
    /// "sha512", "md5". Case-insensitive.
    pub(crate) fn new(hash_type: &str) -> Option<Self> {
        match hash_type.to_lowercase().as_str() {
            "sha-1" | "sha1" => Some(HashState::Sha1(sha1::Sha1::new())),
            "sha-256" | "sha256" => Some(HashState::Sha256(sha2::Sha256::new())),
            "sha-512" | "sha512" => Some(HashState::Sha512(sha2::Sha512::new())),
            "md5" => Some(HashState::Md5(md5::Md5::new())),
            other => {
                trace!("Unsupported hash type for piece verification: {}", other);
                None
            }
        }
    }

    /// Feeds data into the hash computation.
    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            HashState::Sha1(ctx) => Digest::update(ctx, data),
            HashState::Sha256(ctx) => Digest::update(ctx, data),
            HashState::Sha512(ctx) => Digest::update(ctx, data),
            HashState::Md5(ctx) => md5::Digest::update(ctx, data),
        }
    }

    /// Returns the output size in bytes for the hash algorithm.
    #[allow(dead_code)]
    pub(crate) fn output_size(&self) -> usize {
        match self {
            HashState::Sha1(_) => 20,
            HashState::Sha256(_) => 32,
            HashState::Sha512(_) => 64,
            HashState::Md5(_) => 16,
        }
    }
}

/// Finalizes the hash computation, consuming the state and returning the raw
/// hash bytes.
pub(crate) fn finalize_hash(state: HashState) -> Vec<u8> {
    match state {
        HashState::Sha1(ctx) => ctx.finalize().to_vec(),
        HashState::Sha256(ctx) => ctx.finalize().to_vec(),
        HashState::Sha512(ctx) => ctx.finalize().to_vec(),
        HashState::Md5(ctx) => md5::Digest::finalize(ctx).to_vec(),
    }
}
