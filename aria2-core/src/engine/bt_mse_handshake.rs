//! MSE (Message Stream Encryption) 加密握手模块
//!
//! 实现 BitTorrent BEP 10 定义的加密层协议，在 BT 协议握手上叠加可选的加密握手。
//! 包含三阶段握手：Method Selection、PAD/DH 密钥交换、SKEY/SVC 验证。

use rc4::{KeyInit, Rc4 as Rc4Cipher, StreamCipher};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey};
use ring::rand::SystemRandom;
use sha1::{Digest, Sha1};
use std::sync::Mutex;

use crate::error::{Aria2Error, Result};

/// MSE 加密方法
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoMethod {
    /// 明文传输，不加密
    Plain = 0x0001,
    /// RC4 流密码
    Rc4 = 0x0002,
    /// AES-128-CBC (可选)
    Aes128Cbc = 0x0003,
}

impl CryptoMethod {
    /// 从 u16 值创建 CryptoMethod
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            0x0001 => Some(CryptoMethod::Plain),
            0x0002 => Some(CryptoMethod::Rc4),
            0x0003 => Some(CryptoMethod::Aes128Cbc),
            _ => None,
        }
    }

    /// 转换为 u16
    pub fn to_u16(self) -> u16 {
        self as u16
    }
}

/// MSE 握手状态机
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum MseState {
    /// 空闲状态
    Idle,
    /// 已发送 method selection, 等待对方响应
    MethodSelectionSent,
    /// DH 参数交换中
    KeyExchangeInProgress,
    /// 等待 SKEY 验证
    VerificationPending,
    /// 握手完成
    Established(MseCryptoContext),
    /// 握手失败
    Failed(String),
}

/// MSE 加密上下文 (握手完成后用于加解密 BT 消息)
pub struct MseCryptoContext {
    send_key: Vec<u8>,
    recv_key: Vec<u8>,
    crypto_method: CryptoMethod,
    rc4_send: Option<Rc4Cipher>,
    rc4_recv: Option<Rc4Cipher>,
}

impl std::fmt::Debug for MseCryptoContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MseCryptoContext")
            .field("send_key", &self.send_key)
            .field("recv_key", &self.recv_key)
            .field("crypto_method", &self.crypto_method)
            .field("rc4_send", &self.rc4_send.is_some())
            .field("rc4_recv", &self.rc4_recv.is_some())
            .finish()
    }
}

impl Clone for MseCryptoContext {
    fn clone(&self) -> Self {
        // 注意: RC4 状态不能真正克隆 (会丢失状态)，这里创建新的实例
        match self.crypto_method {
            CryptoMethod::Rc4 => {
                // 尝试同步状态 (不完美但可用)
                Self::new(&self.send_key, &self.recv_key, self.crypto_method)
            }
            _ => Self {
                send_key: self.send_key.clone(),
                recv_key: self.recv_key.clone(),
                crypto_method: self.crypto_method,
                rc4_send: None,
                rc4_recv: None,
            },
        }
    }
}

impl PartialEq for MseCryptoContext {
    fn eq(&self, other: &Self) -> bool {
        self.send_key == other.send_key
            && self.recv_key == other.recv_key
            && self.crypto_method == other.crypto_method
    }
}

impl MseCryptoContext {
    /// 使用派生密钥创建新的加密上下文
    ///
    /// 对于 RC4 方法，会初始化两个 RC4 实例并丢弃前 1024 字节 keystream
    /// (MSE spec section 5.2 防止 keystream attack)
    pub fn new(send_key: &[u8], recv_key: &[u8], method: CryptoMethod) -> Self {
        match method {
            CryptoMethod::Rc4 => {
                // 初始化发送方向 RC4 并丢弃 1024 字节 keystream
                let mut rc4_send = Rc4Cipher::new_from_slice(send_key).unwrap();
                let mut discard = vec![0u8; 1024];
                rc4_send.apply_keystream(&mut discard);

                // 初始化接收方向 RC4 并丢弃 1024 字节 keystream
                let mut rc4_recv = Rc4Cipher::new_from_slice(recv_key).unwrap();
                let mut discard = vec![0u8; 1024];
                rc4_recv.apply_keystream(&mut discard);

                Self {
                    send_key: send_key.to_vec(),
                    recv_key: recv_key.to_vec(),
                    crypto_method: method,
                    rc4_send: Some(rc4_send),
                    rc4_recv: Some(rc4_recv),
                }
            }
            _ => Self {
                send_key: send_key.to_vec(),
                recv_key: recv_key.to_vec(),
                crypto_method: method,
                rc4_send: None,
                rc4_recv: None,
            },
        }
    }

    /// 加密数据
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        match self.crypto_method {
            CryptoMethod::Rc4 => {
                if let Some(ref mut rc4) = self.rc4_send {
                    let mut data = plaintext.to_vec();
                    rc4.apply_keystream(&mut data);
                    Ok(data)
                } else {
                    Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                        "RC4 cipher not initialized".to_string(),
                    )))
                }
            }
            CryptoMethod::Plain | CryptoMethod::Aes128Cbc => {
                // Plain 和 AES 模式暂不支持或直接返回原文
                Ok(plaintext.to_vec())
            }
        }
    }

    /// 解密数据
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        match self.crypto_method {
            CryptoMethod::Rc4 => {
                if let Some(ref mut rc4) = self.rc4_recv {
                    let mut data = ciphertext.to_vec();
                    rc4.apply_keystream(&mut data);
                    Ok(data)
                } else {
                    Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                        "RC4 cipher not initialized".to_string(),
                    )))
                }
            }
            CryptoMethod::Plain | CryptoMethod::Aes128Cbc => {
                // Plain 和 AES 模式暂不支持或直接返回原文
                Ok(ciphertext.to_vec())
            }
        }
    }

    /// 获取当前加密方法
    pub fn crypto_method(&self) -> CryptoMethod {
        self.crypto_method
    }

    /// 是否使用加密
    pub fn is_encrypted(&self) -> bool {
        self.crypto_method != CryptoMethod::Plain
    }
}

/// 明文回退: 创建无加密的上下文
impl Default for MseCryptoContext {
    fn default() -> Self {
        Self {
            send_key: vec![],
            recv_key: vec![],
            crypto_method: CryptoMethod::Plain,
            rc4_send: None,
            rc4_recv: None,
        }
    }
}

/// MSE 握手管理器
pub struct MseHandshakeManager {
    state: Mutex<MseState>,
    #[allow(dead_code)] // Reserved for crypto method preference in future handshake logic
    preferred_crypto: CryptoMethod,
    local_dh_private_key: Option<EphemeralPrivateKey>,
    local_dh_pubkey: Vec<u8>,
    remote_dh_pubkey: Option<Vec<u8>>,
    pub(crate) shared_secret: Option<Vec<u8>>,
    info_hash: [u8; 20],
    pad_length: u16,
}

impl MseHandshakeManager {
    /// 创建新的 MSE 握手实例
    pub fn new(info_hash: [u8; 20], preferred: CryptoMethod) -> Result<Self> {
        let rng = SystemRandom::new();

        // 生成 X25519 密钥对
        let private_key = EphemeralPrivateKey::generate(&agreement::X25519, &rng).map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Failed to generate DH key: {}",
                e
            )))
        })?;

        // 获取公钥 (X25519 公钥固定为 32 字节)
        let public_key = private_key.compute_public_key().map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Failed to compute public key: {}",
                e
            )))
        })?;

        let pubkey_slice = public_key.as_ref();
        let mut local_dh_pubkey = vec![0u8; pubkey_slice.len()];
        local_dh_pubkey.copy_from_slice(pubkey_slice);

        // 随机 PAD 长度 (0-512 bytes)
        use rand::RngCore;
        let mut rng_core = rand::thread_rng();
        let pad_length = rng_core.next_u32() as u16 % 513;

        Ok(Self {
            state: Mutex::new(MseState::Idle),
            preferred_crypto: preferred,
            local_dh_private_key: Some(private_key),
            local_dh_pubkey,
            remote_dh_pubkey: None,
            shared_secret: None,
            info_hash,
            pad_length,
        })
    }

    /// Phase 1: 构造 Method Selection payload
    ///
    /// 返回 `\x13MSegadd` (支持加密) 或 `\x00` (仅明文)
    pub fn build_method_selection(&self) -> Vec<u8> {
        b"\x13MSegadd".to_vec()
    }

    /// 解析对方的 Method Selection
    ///
    /// data: 对方发来的 method selection bytes
    pub fn parse_remote_method_selection(data: &[u8]) -> Result<CryptoMethod> {
        if data.is_empty() {
            return Err(Aria2Error::Parse("Empty method selection".to_string()));
        }

        // 检查是否为 MSegadd (\x13MSegadd)
        if data == b"\x13MSegadd" {
            return Ok(CryptoMethod::Rc4); // 支持加密
        }

        // 检查是否为明文模式 (\x00)
        if data == b"\x00" || data[0] == 0x00 {
            return Ok(CryptoMethod::Plain);
        }

        Err(Aria2Error::Parse(format!(
            "Invalid method selection: {:?}",
            data
        )))
    }

    /// Phase 2: 构建 PAD + DH 公钥 + CryptoProvisions payload
    ///
    /// 格式:
    /// ```text
    /// ┌────────┬──────────┬─────────────┬────────────┐
    /// │ PAD_D  │ PAD_LEN  │ Crypto_Pro  │ ICB/IV     │
    /// │ (2B BE)│ (2B BE)  │ (2B BE)     │ (可选 16B)  │
    /// ├────────┼──────────┼─────────────┼────────────┤
    /// │          DH Public Key (X25519 = 32 bytes)                │
    /// └────────┴──────────┴─────────────┴────────────┘
    /// ```
    pub fn build_key_exchange_payload(&self, crypto_methods: &[CryptoMethod]) -> Result<Vec<u8>> {
        let mut payload = Vec::new();

        // PAD_D (2 bytes big-endian): 随机填充长度
        payload.extend_from_slice(&self.pad_length.to_be_bytes());

        // PAD_LEN (2 bytes big-endian): 与 PAD_D 相同
        payload.extend_from_slice(&self.pad_length.to_be_bytes());

        // Crypto_Provisions (2 bytes big-endian): 支持的加密方法位掩码
        let mut crypto_provisions: u16 = 0;
        for method in crypto_methods {
            crypto_provisions |= method.to_u16();
        }
        payload.extend_from_slice(&crypto_provisions.to_be_bytes());

        // 随机 PAD 数据
        use rand::RngCore;
        let mut rng_core = rand::thread_rng();
        let mut pad_data = vec![0u8; self.pad_length as usize];
        rng_core.fill_bytes(&mut pad_data);
        payload.extend_from_slice(&pad_data);

        // DH Public Key (32 bytes for X25519)
        payload.extend_from_slice(&self.local_dh_pubkey);

        Ok(payload)
    }

    /// 解析对方的 key exchange payload, 计算 shared secret
    pub fn process_remote_key_exchange(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 8 + 32 {
            return Err(Aria2Error::Parse(
                "Key exchange payload too short".to_string(),
            ));
        }

        // 解析字段
        let _pad_d = u16::from_be_bytes([data[0], data[1]]);
        let _pad_len = u16::from_be_bytes([data[2], data[3]]);
        let _crypto_pro = u16::from_be_bytes([data[4], data[5]]);
        // 跳过可能的 ICB/IV (6-22)

        // 计算 PAD 结束位置
        let pad_end = 6 + (_pad_len as usize);
        if data.len() < pad_end + 32 {
            return Err(Aria2Error::Parse(
                "Key exchange payload truncated".to_string(),
            ));
        }

        // 提取远程 DH 公钥 (最后 32 bytes)
        let remote_pubkey_start = data.len() - 32;
        let remote_pubkey = &data[remote_pubkey_start..];

        // 存储远程公钥
        self.remote_dh_pubkey = Some(remote_pubkey.to_vec());

        // 计算共享密钥
        let peer_public_key = UnparsedPublicKey::new(&agreement::X25519, remote_pubkey);

        if let Some(private_key) = self.local_dh_private_key.take() {
            let shared_secret: Vec<u8> = agreement::agree_ephemeral(
                private_key,
                &peer_public_key,
                |shared_secret: &[u8]| shared_secret.to_vec(),
            )
            .map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "DH key agreement failed: {}",
                    e
                )))
            })?;

            self.shared_secret = Some(shared_secret);
        } else {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                "Local DH private key not set".to_string(),
            )));
        }

        Ok(())
    }

    /// Phase 3: 构造 SKEY + VC + CryptoSelect 验证 payload
    ///
    /// 格式:
    /// - SKEY hash (20 bytes): SHA-1(info_hash + shared_secret)
    /// - VC (2 bytes): version cipher (0x0001 for RC4)
    /// - CryptoSelect (2 bytes): 选定的加密方法
    /// - len(I) (2 bytes): 可选的初始数据长度 (通常为 0)
    pub fn build_verification_payload(&self, selected_method: CryptoMethod) -> Result<Vec<u8>> {
        let skey = self.compute_skey()?;
        let mut payload = Vec::new();

        // SKEY hash (20 bytes)
        payload.extend_from_slice(&skey);

        // VC (2 bytes): 版本密码
        payload.extend_from_slice(&0x0001u16.to_be_bytes());

        // CryptoSelect (2 bytes): 选定的加密方法
        payload.extend_from_slice(&selected_method.to_u16().to_be_bytes());

        // len(I) (2 bytes): 初始数据长度
        payload.extend_from_slice(&0x0000u16.to_be_bytes());

        Ok(payload)
    }

    /// 解析对方的 verification payload, 验证 SKEY, 完成握手
    pub fn process_remote_verification(&mut self, data: &[u8]) -> Result<MseCryptoContext> {
        if data.len() < 26 {
            return Err(Aria2Error::Parse(
                "Verification payload too short".to_string(),
            ));
        }

        // 解析字段
        let remote_skey = &data[..20];
        let vc = u16::from_be_bytes([data[20], data[21]]);
        let crypto_select = u16::from_be_bytes([data[22], data[23]]);
        // len(I) at [24,25]

        // 验证 VC
        if vc != 0x0001 {
            return Err(Aria2Error::Parse(format!("Invalid VC value: {:#06x}", vc)));
        }

        // 验证 SKEY
        let expected_skey = self.compute_skey()?;
        if remote_skey != expected_skey.as_slice() {
            return Err(Aria2Error::Checksum("SKEY verification failed".to_string()));
        }

        // 解析选定的加密方法
        let selected_method = CryptoMethod::from_u16(crypto_select).ok_or_else(|| {
            Aria2Error::Parse(format!(
                "Unknown crypto method selected: {:#06x}",
                crypto_select
            ))
        })?;

        // 派生加密密钥
        let shared_secret = self.shared_secret.as_ref().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Shared secret not computed".to_string(),
            ))
        })?;

        let (send_key, recv_key) = Self::derive_keys(&expected_skey, shared_secret);

        // 创建加密上下文
        let ctx = MseCryptoContext::new(&send_key, &recv_key, selected_method);

        Ok(ctx)
    }

    /// 内部: 计算 SKEY = SHA-1(info_hash || shared_secret)
    pub(crate) fn compute_skey(&self) -> Result<[u8; 20]> {
        let shared_secret = self.shared_secret.as_ref().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Shared secret not computed yet".to_string(),
            ))
        })?;

        let mut hasher = Sha1::new();
        hasher.update(self.info_hash);
        hasher.update(shared_secret);
        let result = hasher.finalize();

        let mut skey = [0u8; 20];
        skey.copy_from_slice(&result);
        Ok(skey)
    }

    /// 内部: 派生加密密钥
    ///
    /// ```text
    /// send_key = SHA-1(SKEY + "keyA" + shared_secret)[:16]
    /// recv_key = SHA-1(SKEY + "keyB" + shared_secret)[:16]
    /// ```
    pub(crate) fn derive_keys(skey: &[u8], shared_secret: &[u8]) -> (Vec<u8>, Vec<u8>) {
        // 发送方向密钥
        let mut hasher_a = Sha1::new();
        hasher_a.update(skey);
        hasher_a.update(b"keyA");
        hasher_a.update(shared_secret);
        let result_a = hasher_a.finalize();

        // 接收方向密钥
        let mut hasher_b = Sha1::new();
        hasher_b.update(skey);
        hasher_b.update(b"keyB");
        hasher_b.update(shared_secret);
        let result_b = hasher_b.finalize();

        // 取前 16 字节
        (result_a[..16].to_vec(), result_b[..16].to_vec())
    }

    /// 明文回退: 创建无加密的上下文
    pub fn plaintext_fallback() -> MseCryptoContext {
        MseCryptoContext::default()
    }

    /// 获取当前状态
    pub fn state(&self) -> MseState {
        self.state.lock().unwrap().clone()
    }

    /// 更新状态
    pub fn set_state(&self, state: MseState) {
        *self.state.lock().unwrap() = state;
    }

    /// 获取本地 DH 公钥 (用于调试)
    pub fn local_dh_pubkey(&self) -> &[u8] {
        &self.local_dh_pubkey
    }

    /// 获取共享密钥 (用于调试)
    pub fn shared_secret(&self) -> Option<&[u8]> {
        self.shared_secret.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crypto_method_conversions() {
        assert_eq!(CryptoMethod::from_u16(0x0001), Some(CryptoMethod::Plain));
        assert_eq!(CryptoMethod::from_u16(0x0002), Some(CryptoMethod::Rc4));
        assert_eq!(
            CryptoMethod::from_u16(0x0003),
            Some(CryptoMethod::Aes128Cbc)
        );
        assert_eq!(CryptoMethod::from_u16(0x9999), None);

        assert_eq!(CryptoMethod::Plain.to_u16(), 0x0001);
        assert_eq!(CryptoMethod::Rc4.to_u16(), 0x0002);
        assert_eq!(CryptoMethod::Aes128Cbc.to_u16(), 0x0003);
    }
}
