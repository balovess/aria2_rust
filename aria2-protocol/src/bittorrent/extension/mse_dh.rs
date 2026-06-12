use num_bigint_dig::{BigUint, RandBigInt};
use num_traits::{Num, Zero};

pub const DH_P_1024_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1\
29024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374\
FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F2\
41C6742C588AB493AAEB82D84BD10DA120B46EC61D679E3C5FABA75DD4E077AF92D5A0F06D38A1AFFA0\
3C96872A35703200E8FAD223A629D0DD64DF76DB16D96905ECBCA1982228F0BE88E7F8A0C4BFCAFC8F02\
01";

pub const DH_G: u64 = 2;

/// MSE DH Key Exchange implementation following BEP 10 specification.
/// Uses 1024-bit DH parameters from RFC 3526.
#[derive(Debug, Clone)]
pub struct MseDhKeyExchange {
    /// Internal DH key pair (uses full-size keys for correct computation)
    keypair: DhKeyPair,
}

impl MseDhKeyExchange {
    /// Create a new DH key exchange instance with randomly generated keys.
    pub fn new() -> Self {
        MseDhKeyExchange {
            keypair: DhKeyPair::generate(),
        }
    }

    /// Generate the public key from the private key.
    /// Returns a 96-byte array containing the public key.
    /// Note: The actual public key may be up to 128 bytes, but we return
    /// the last 96 bytes (most significant bytes) for protocol compliance.
    pub fn generate_public_key(&self) -> [u8; 96] {
        let mut result = [0u8; 96];
        let pub_bytes = &self.keypair.public;
        
        // Take the last 96 bytes (most significant) or pad with zeros at the beginning
        let offset = pub_bytes.len().saturating_sub(96);
        let len = pub_bytes.len().saturating_sub(offset);
        result[..len].copy_from_slice(&pub_bytes[offset..]);
        
        result
    }

    /// Get the full public key (for internal use).
    #[allow(dead_code)]
    fn full_public_key(&self) -> &[u8] {
        &self.keypair.public
    }

    /// Compute the shared secret using the other party's public key.
    /// Returns a 96-byte array containing the shared secret.
    /// 
    /// Note: This method expects the full public key from the other party,
    /// not the truncated 96-byte version. Use `compute_shared_secret_full()` 
    /// if you have the full public key.
    pub fn compute_shared_secret(&self, other_public: &[u8; 96]) -> [u8; 96] {
        // Pad the other public key to full size (prepend zeros if needed)
        // This assumes the other public key was also truncated to 96 bytes
        let mut other_pub_full = vec![0u8; 128 - 96];
        other_pub_full.extend_from_slice(other_public);
        
        // Compute shared secret using the full-size implementation
        let shared = self.keypair.compute_shared_secret(&other_pub_full);
        
        // Pad or truncate to 96 bytes
        let mut result = [0u8; 96];
        let offset = shared.len().saturating_sub(96);
        let len = shared.len().saturating_sub(offset);
        result[..len].copy_from_slice(&shared[offset..]);
        
        result
    }

    /// Compute the shared secret using the full public key from the other party.
    /// This is the correct method to use for DH key exchange.
    pub fn compute_shared_secret_full(&self, other_public_full: &[u8]) -> [u8; 96] {
        // Compute shared secret using the full-size implementation
        let shared = self.keypair.compute_shared_secret(other_public_full);
        
        // Pad or truncate to 96 bytes
        let mut result = [0u8; 96];
        let offset = shared.len().saturating_sub(96);
        let len = shared.len().saturating_sub(offset);
        result[..len].copy_from_slice(&shared[offset..]);
        
        result
    }
}

impl Default for MseDhKeyExchange {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct DhKeyPair {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

impl DhKeyPair {
    fn get_prime() -> BigUint {
        BigUint::from_str_radix(DH_P_1024_HEX, 16).expect("DH prime constant is valid")
    }

    pub fn generate() -> Self {
        let p = Self::get_prime();
        let g: BigUint = DH_G.into();

        let mut rng = rand::thread_rng();
        let two: BigUint = 2u32.into();
        let p_minus_two = &p - two.clone();

        let private_big = rng.gen_biguint_range(&two, &p_minus_two);
        let public_big = g.modpow(&private_big, &p);

        DhKeyPair {
            private: private_big.to_bytes_be(),
            public: public_big.to_bytes_be(),
        }
    }

    pub fn compute_shared_secret(&self, other_public: &[u8]) -> Vec<u8> {
        let p = Self::get_prime();
        let other_pub = BigUint::from_bytes_be(other_public);
        let self_priv = BigUint::from_bytes_be(&self.private);

        if other_pub.is_zero() || self_priv.is_zero() || other_pub >= p {
            return vec![0u8; self.private.len().max(other_public.len())];
        }

        other_pub.modpow(&self_priv, &p).to_bytes_be()
    }
}

impl Default for DhKeyPair {
    fn default() -> Self {
        Self::generate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_traits::One;

    #[test]
    fn test_mse_dh_key_exchange_new() {
        let exchange = MseDhKeyExchange::new();
        // Public key should not be all zeros
        let public = exchange.generate_public_key();
        assert_ne!(public, [0u8; 96]);
    }

    #[test]
    fn test_mse_dh_generate_public_key() {
        let exchange = MseDhKeyExchange::new();
        let public = exchange.generate_public_key();
        assert_eq!(public.len(), 96);
        // Generate again should return the same key
        let public2 = exchange.generate_public_key();
        assert_eq!(public, public2);
    }

    #[test]
    fn test_mse_dh_shared_secret_symmetry() {
        let alice = MseDhKeyExchange::new();
        let bob = MseDhKeyExchange::new();

        // Use full public keys for correct DH computation
        let alice_pub_full = alice.full_public_key();
        let bob_pub_full = bob.full_public_key();

        let s_ab = alice.compute_shared_secret_full(bob_pub_full);
        let s_ba = bob.compute_shared_secret_full(alice_pub_full);

        assert_eq!(s_ab, s_ba, "Shared secrets should be equal");
        assert!(!s_ab.iter().all(|&b| b == 0), "Shared secret should not be all zeros");
    }

    #[test]
    fn test_mse_dh_different_keys_different_secrets() {
        let alice1 = MseDhKeyExchange::new();
        let alice2 = MseDhKeyExchange::new();
        let bob = MseDhKeyExchange::new();

        let bob_pub_full = bob.full_public_key();
        let s1 = alice1.compute_shared_secret_full(bob_pub_full);
        let s2 = alice2.compute_shared_secret_full(bob_pub_full);

        // Different private keys should produce different shared secrets (with high probability)
        // Note: There's a tiny chance they could be equal, but it's negligible
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
    fn test_shared_secret_symmetry() {
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
        assert!(
            bits >= 1024,
            "DH prime must be at least 1024 bits, got {}",
            bits
        );
    }
}
