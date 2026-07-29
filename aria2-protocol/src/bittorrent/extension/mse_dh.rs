//! MSE DH Key Exchange using the 768-bit prime from the MSE specification.
//!
//! Matches C++ aria2 `InternalDHKeyExchange.cc` which uses:
//! - PRIME_BITS = 768, KEY_LENGTH = 96 bytes
//! - Generator = 2
//! - Private key bits = 160

use num_bigint_dig::BigUint;
use num_traits::{Num, Zero};

/// MSE 768-bit prime from C++ aria2 MSEHandshake.cc.
/// This is NOT RFC 3526; it is the original MSE spec prime.
pub const DH_P_768_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1\
29024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374\
FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563";

/// DH generator value (2, same as C++ aria2).
pub const DH_G: u64 = 2;

/// Prime bit length (768 bits, matching C++ `PRIME_BITS`).
pub const PRIME_BITS: usize = 768;

/// Key length in bytes = (768 + 7) / 8 = 96, matching C++ `KEY_LENGTH`.
pub const KEY_LENGTH: usize = (PRIME_BITS + 7) / 8; // 96

/// Private key length in bits, matching C++ `init(PRIME, PRIME_BITS, GENERATOR, 160)`.
pub const PRIVATE_KEY_BITS: usize = 160;

/// Maximum padding length, matching C++ `MAX_PAD_LENGTH`.
pub const MAX_PAD_LENGTH: usize = 512;

/// VC (Verification Constant) length in bytes.
pub const VC_LENGTH: usize = 8;

/// Crypto bitfield length in bytes (4 bytes, big-endian u32).
pub const CRYPTO_BITFIELD_LENGTH: usize = 4;

/// Info hash length in bytes.
pub const INFO_HASH_LENGTH: usize = 20;

/// SHA-1 digest length in bytes.
pub const SHA1_LENGTH: usize = 20;

/// MSE DH Key Exchange implementation matching C++ aria2.
/// Uses the 768-bit MSE prime with generator 2.
#[derive(Debug, Clone)]
pub struct MseDhKeyExchange {
    /// Internal DH key pair
    keypair: DhKeyPair,
}

impl MseDhKeyExchange {
    /// Create a new DH key exchange instance with randomly generated keys.
    pub fn new() -> Self {
        MseDhKeyExchange {
            keypair: DhKeyPair::generate(),
        }
    }

    /// Generate the public key as a fixed-size 96-byte big-endian array.
    ///
    /// The output is left-padded with zeros to exactly `KEY_LENGTH` bytes,
    /// matching C++ `DHKeyExchange::getPublicKey()` which writes the
    /// big-endian bignum into a fixed `KEY_LENGTH` buffer.
    pub fn generate_public_key(&self) -> [u8; KEY_LENGTH] {
        self.keypair.public_fixed()
    }

    /// Get the variable-length raw public key bytes (for internal use).
    #[allow(dead_code)]
    pub fn raw_public_key(&self) -> &[u8] {
        &self.keypair.public
    }

    /// Compute the shared secret using the other party's public key.
    ///
    /// `other_public` must be exactly `KEY_LENGTH` (96) bytes in big-endian.
    /// Returns a fixed-size 96-byte array matching C++ `computeSecret()`.
    pub fn compute_shared_secret(&self, other_public: &[u8; KEY_LENGTH]) -> [u8; KEY_LENGTH] {
        let shared = self.keypair.compute_shared_secret(other_public);
        fixed_size_bytes::<KEY_LENGTH>(&shared)
    }

    /// Compute the shared secret from a variable-length public key.
    /// The input may be shorter or longer than KEY_LENGTH; it is
    /// interpreted as a big-endian unsigned integer.
    pub fn compute_shared_secret_varlen(&self, other_public: &[u8]) -> [u8; KEY_LENGTH] {
        let shared = self.keypair.compute_shared_secret(other_public);
        fixed_size_bytes::<KEY_LENGTH>(&shared)
    }
}

impl Default for MseDhKeyExchange {
    fn default() -> Self {
        Self::new()
    }
}

/// DH key pair with 768-bit MSE prime.
#[derive(Debug, Clone)]
pub struct DhKeyPair {
    /// Private key as big-endian bytes (variable length).
    pub private: Vec<u8>,
    /// Public key as big-endian bytes (variable length).
    pub public: Vec<u8>,
}

impl DhKeyPair {
    /// Get the 768-bit MSE prime as a BigUint.
    pub fn get_prime() -> BigUint {
        BigUint::from_str_radix(DH_P_768_HEX, 16)
            .expect("DH prime constant is valid hex")
    }

    /// Generate a new DH key pair.
    ///
    /// Private key is 160 random bits (matching C++ `privateKeyBits = 160`),
    /// and public key = g^private mod p.
    pub fn generate() -> Self {
        let p = Self::get_prime();
        let g: BigUint = DH_G.into();

        // Generate a 160-bit random private key, matching C++ aria2 behavior:
        //   size_t pbytes = (privateKeyBits + 7) / 8;  // = 20
        //   util::generateRandomData(buf, pbytes);
        //   privateKey_ = n(buf, pbytes);
        let private_bytes = (PRIVATE_KEY_BITS + 7) / 8; // 20 bytes
        let private_big = {
            use rand::RngCore;
            let mut buf = vec![0u8; private_bytes];
            rand::thread_rng().fill_bytes(&mut buf);
            BigUint::from_bytes_be(&buf)
        };

        let public_big = g.modpow(&private_big, &p);

        DhKeyPair {
            private: private_big.to_bytes_be(),
            public: public_big.to_bytes_be(),
        }
    }

    /// Compute the shared secret: `other_public^private mod p`.
    ///
    /// Returns the big-endian bytes of the shared secret.
    /// The result may be shorter than KEY_LENGTH if the secret has
    /// leading zero bytes when represented as a fixed-size integer.
    pub fn compute_shared_secret(&self, other_public: &[u8]) -> Vec<u8> {
        let p = Self::get_prime();
        let other_pub = BigUint::from_bytes_be(other_public);
        let self_priv = BigUint::from_bytes_be(&self.private);

        // Guard against degenerate inputs
        if other_pub.is_zero() || self_priv.is_zero() || other_pub >= p {
            let len = self.private.len().max(other_public.len());
            return vec![0u8; len];
        }

        other_pub.modpow(&self_priv, &p).to_bytes_be()
    }

    /// Return the public key as a fixed-size 96-byte array.
    /// Left-padded with zeros, matching C++ `getPublicKey()`.
    pub fn public_fixed(&self) -> [u8; KEY_LENGTH] {
        fixed_size_bytes::<KEY_LENGTH>(&self.public)
    }
}

impl Default for DhKeyPair {
    fn default() -> Self {
        Self::generate()
    }
}

/// Left-pad or truncate a big-endian byte slice to exactly N bytes,
/// matching C++ bignum `binary(out, outLength)` which zero-fills
/// the output buffer and writes the big-endian value at the end.
pub fn fixed_size_bytes<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut result = [0u8; N];
    let offset = bytes.len().saturating_sub(N);
    let len = bytes.len().saturating_sub(offset);
    result[N - len..].copy_from_slice(&bytes[offset..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mse_dh_key_exchange_new() {
        let exchange = MseDhKeyExchange::new();
        let public = exchange.generate_public_key();
        // Public key should not be all zeros
        assert_ne!(public, [0u8; KEY_LENGTH]);
    }

    #[test]
    fn test_mse_dh_generate_public_key_size() {
        let exchange = MseDhKeyExchange::new();
        let public = exchange.generate_public_key();
        assert_eq!(public.len(), KEY_LENGTH, "Public key must be exactly 96 bytes");
        // Generate again should return the same key
        let public2 = exchange.generate_public_key();
        assert_eq!(public, public2);
    }

    #[test]
    fn test_mse_dh_shared_secret_symmetry() {
        let alice = MseDhKeyExchange::new();
        let bob = MseDhKeyExchange::new();

        let alice_pub = alice.generate_public_key();
        let bob_pub = bob.generate_public_key();

        let s_ab = alice.compute_shared_secret(&bob_pub);
        let s_ba = bob.compute_shared_secret(&alice_pub);

        assert_eq!(s_ab, s_ba, "Shared secrets must be equal");
        assert!(
            !s_ab.iter().all(|&b| b == 0),
            "Shared secret should not be all zeros"
        );
    }

    #[test]
    fn test_mse_dh_different_keys_different_secrets() {
        let alice1 = MseDhKeyExchange::new();
        let alice2 = MseDhKeyExchange::new();
        let bob = MseDhKeyExchange::new();

        let bob_pub = bob.generate_public_key();
        let s1 = alice1.compute_shared_secret(&bob_pub);
        let s2 = alice2.compute_shared_secret(&bob_pub);

        assert_ne!(s1, s2, "Different keys should produce different secrets");
    }

    #[test]
    fn test_generate_keypair() {
        let pair = DhKeyPair::generate();
        assert!(!pair.private.is_empty());
        assert!(!pair.public.is_empty());
        assert_ne!(pair.private, vec![0u8; pair.private.len()]);
        assert_ne!(pair.public, vec![0u8; pair.public.len()]);
    }

    #[test]
    fn test_shared_secret_symmetry_raw() {
        let alice = DhKeyPair::generate();
        let bob = DhKeyPair::generate();

        let s_ab = alice.compute_shared_secret(&bob.public);
        let s_ba = bob.compute_shared_secret(&alice.public);

        assert_eq!(s_ab, s_ba);
        assert!(!s_ab.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_different_pairs_different_secrets() {
        let a1 = DhKeyPair::generate();
        let a2 = DhKeyPair::generate();
        let b = DhKeyPair::generate();

        let s1 = a1.compute_shared_secret(&b.public);
        let s2 = a2.compute_shared_secret(&b.public);

        assert_ne!(s1, s2);
    }

    #[test]
    fn test_public_key_in_valid_range() {
        for _ in 0..5 {
            let pair = DhKeyPair::generate();
            let pub_val = BigUint::from_bytes_be(&pair.public);
            let one: BigUint = One::one();
            let p = DhKeyPair::get_prime();
            assert!(
                pub_val > one && pub_val < p,
                "public key must be in range (1, p)"
            );
        }
    }

    #[test]
    fn test_prime_constant_bit_length() {
        let p = DhKeyPair::get_prime();
        let bits = p.bits();
        assert_eq!(
            bits, 768,
            "DH prime must be exactly 768 bits, got {}",
            bits
        );
    }

    #[test]
    fn test_prime_matches_cpp() {
        // Verify the prime matches the C++ constant exactly
        let p = DhKeyPair::get_prime();
        let hex = format!("{:X}", p);
        assert_eq!(hex, DH_P_768_HEX.replace('\\', "").replace('\n', ""));
    }

    #[test]
    fn test_fixed_size_bytes_padding() {
        // Shorter input should be left-padded with zeros
        let result = fixed_size_bytes::<4>(&[0x01, 0x02]);
        assert_eq!(result, [0x00, 0x00, 0x01, 0x02]);

        // Exact length
        let result = fixed_size_bytes::<4>(&[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(result, [0x01, 0x02, 0x03, 0x04]);

        // All zeros
        let result = fixed_size_bytes::<2>(&[0x00]);
        assert_eq!(result, [0x00, 0x00]);
    }
}
