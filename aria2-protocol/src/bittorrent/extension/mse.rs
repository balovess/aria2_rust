#[derive(Debug, Clone)]
pub struct MseSession {
    pub encrypted: bool,
    pub crypto_method: MseCryptoMethod,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MseCryptoMethod {
    Plain,
    Rc4,
    Aes128Cfb,
}

impl Default for MseSession {
    fn default() -> Self {
        Self {
            encrypted: false,
            crypto_method: MseCryptoMethod::Plain,
        }
    }
}

impl MseSession {
    pub fn new_plain() -> Self {
        Self::default()
    }

    pub fn new_rc4() -> Self {
        Self {
            encrypted: true,
            crypto_method: MseCryptoMethod::Rc4,
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    pub fn handshake_supported(reserved_bytes: &[u8; 8]) -> bool {
        reserved_bytes[0] & 0x01 != 0
    }

    pub fn select_crypto_method(
        _local_methods: &[MseCryptoMethod],
        _remote_methods: &[MseCryptoMethod],
    ) -> Option<MseCryptoMethod> {
        Some(MseCryptoMethod::Plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mse_session_defaults() {
        let session = MseSession::new_plain();
        assert!(!session.is_encrypted());
        assert_eq!(session.crypto_method, MseCryptoMethod::Plain);
    }

    #[test]
    fn test_mse_reserved_bytes_detection() {
        let mut with_mse = [0u8; 8];
        with_mse[0] = 0x01;
        assert!(MseSession::handshake_supported(&with_mse));

        let without_mse = [0u8; 8];
        assert!(!MseSession::handshake_supported(&without_mse));
    }
}
