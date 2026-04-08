use num_bigint_dig::{BigUint, RandBigInt};
use num_traits::{One, Zero, Num};
use rand::Rng;

pub const DH_P_1024_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1\
29024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374\
FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F2\
41C6742C588AB493AAEB82D84BD10DA120B46EC61D679E3C5FABA75DD4E077AF92D5A0F06D38A1AFFA0\
3C96872A35703200E8FAD223A629D0DD64DF76DB16D96905ECBCA1982228F0BE88E7F8A0C4BFCAFC8F02\
01";

pub const DH_G: u64 = 2;

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
            assert!(pub_val > one && pub_val < p,
                "public key must be in range (1, p)");
        }
    }

    #[test]
    fn test_prime_constant_bit_length() {
        let p = DhKeyPair::get_prime();
        let bits = p.bits();
        assert!(bits >= 1024, "DH prime must be at least 1024 bits, got {}", bits);
    }
}
