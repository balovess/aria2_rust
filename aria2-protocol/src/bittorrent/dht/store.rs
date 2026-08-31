//! In-memory BEP 44 item store.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use super::modern::{MutableUpdate, StoredItem, accept_mutable_update};
use crate::bittorrent::bencode::codec::BencodeValue;

const MAX_VALUE_BYTES: usize = 1000;
const MAX_SALT_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    ValueTooLarge,
    SaltTooLarge,
    InvalidTarget,
    InvalidSignature,
    SequenceTooLow,
    CasMismatch,
}

#[derive(Clone, Default)]
pub struct DhtItemStore {
    items: Arc<RwLock<HashMap<[u8; 20], StoredItem>>>,
}

impl DhtItemStore {
    pub fn targets(&self) -> Vec<[u8; 20]> {
        self.items
            .read()
            .map(|items| items.keys().copied().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, target: &[u8; 20]) -> Option<StoredItem> {
        self.items.read().ok()?.get(target).cloned()
    }

    pub fn put_immutable(&self, value: BencodeValue) -> Result<[u8; 20], StoreError> {
        if value.encode().len() > MAX_VALUE_BYTES {
            return Err(StoreError::ValueTooLarge);
        }
        let target = StoredItem::immutable_target(&value);
        self.items
            .write()
            .map_err(|_| StoreError::InvalidTarget)?
            .insert(target, StoredItem::Immutable { target, value });
        Ok(target)
    }

    pub fn put_mutable(
        &self,
        item: super::modern::MutableValue,
        cas: Option<i64>,
    ) -> Result<[u8; 20], StoreError> {
        if item.value.encode().len() > MAX_VALUE_BYTES {
            return Err(StoreError::ValueTooLarge);
        }
        if item
            .salt
            .as_deref()
            .is_some_and(|salt| salt.len() > MAX_SALT_BYTES)
        {
            return Err(StoreError::SaltTooLarge);
        }
        if item.sequence < 0 || !item.verify_signature() {
            return Err(StoreError::InvalidSignature);
        }
        let target = StoredItem::mutable_target(&item.public_key, item.salt.as_deref());
        let mut items = self.items.write().map_err(|_| StoreError::InvalidTarget)?;
        let current = items.get(&target).and_then(|stored| match stored {
            StoredItem::Mutable { item, .. } => Some(item),
            StoredItem::Immutable { .. } => None,
        });
        match accept_mutable_update(current, &item, cas) {
            MutableUpdate::Accepted => {
                items.insert(target, StoredItem::Mutable { target, item });
                Ok(target)
            }
            MutableUpdate::RejectedSequence => Err(StoreError::SequenceTooLow),
            MutableUpdate::RejectedCas => Err(StoreError::CasMismatch),
            MutableUpdate::RejectedSignature => Err(StoreError::InvalidSignature),
        }
    }

    /// Serialize the local BEP 44 store as a bencoded list. The format is
    /// deliberately independent from aria2's historical `dht.dat` routing
    /// table format so old files remain readable by older clients.
    pub fn serialize(&self) -> Result<Vec<u8>, String> {
        let items = self
            .items
            .read()
            .map_err(|_| "BEP 44 item store lock poisoned".to_string())?;
        let values = items
            .values()
            .map(|item| {
                let mut entry = std::collections::BTreeMap::new();
                match item {
                    StoredItem::Immutable { target, value } => {
                        entry.insert(b"kind".to_vec(), BencodeValue::Bytes(b"immutable".to_vec()));
                        entry.insert(b"target".to_vec(), BencodeValue::Bytes(target.to_vec()));
                        entry.insert(b"v".to_vec(), value.clone());
                    }
                    StoredItem::Mutable { target, item } => {
                        entry.insert(b"kind".to_vec(), BencodeValue::Bytes(b"mutable".to_vec()));
                        entry.insert(b"target".to_vec(), BencodeValue::Bytes(target.to_vec()));
                        entry.insert(b"k".to_vec(), BencodeValue::Bytes(item.public_key.to_vec()));
                        entry.insert(
                            b"sig".to_vec(),
                            BencodeValue::Bytes(item.signature.to_vec()),
                        );
                        entry.insert(b"seq".to_vec(), BencodeValue::Int(item.sequence));
                        if let Some(salt) = &item.salt {
                            entry.insert(b"salt".to_vec(), BencodeValue::Bytes(salt.clone()));
                        }
                        entry.insert(b"v".to_vec(), item.value.clone());
                    }
                }
                BencodeValue::Dict(entry)
            })
            .collect::<Vec<_>>();
        Ok(BencodeValue::List(values).encode())
    }

    /// Restore a store from [`Self::serialize`], rejecting malformed or
    /// cryptographically invalid entries instead of silently accepting them.
    pub fn deserialize(data: &[u8]) -> Result<Self, String> {
        let (value, consumed) = BencodeValue::decode(data)?;
        if consumed != data.len() {
            return Err("BEP 44 item store has trailing data".to_string());
        }
        let entries = value
            .as_list()
            .ok_or("BEP 44 item store root must be a list")?;
        let store = Self::default();
        for entry in entries {
            let dict = entry
                .as_dict()
                .ok_or("BEP 44 item entry must be a dictionary")?;
            let target: [u8; 20] = dict
                .get(&b"target"[..])
                .and_then(BencodeValue::as_bytes)
                .ok_or("BEP 44 item entry missing target")?
                .try_into()
                .map_err(|_| "BEP 44 item target must be 20 bytes")?;
            let kind = dict
                .get(&b"kind"[..])
                .and_then(BencodeValue::as_bytes)
                .ok_or("BEP 44 item entry missing kind")?;
            let value = dict
                .get(&b"v"[..])
                .cloned()
                .ok_or("BEP 44 item entry missing value")?;
            match kind {
                b"immutable" => {
                    if StoredItem::immutable_target(&value) != target {
                        return Err("BEP 44 immutable target mismatch".to_string());
                    }
                    store
                        .items
                        .write()
                        .map_err(|_| "BEP 44 item store lock poisoned".to_string())?
                        .insert(target, StoredItem::Immutable { target, value });
                }
                b"mutable" => {
                    let public_key: [u8; 32] = dict
                        .get(&b"k"[..])
                        .and_then(BencodeValue::as_bytes)
                        .ok_or("BEP 44 mutable item missing public key")?
                        .try_into()
                        .map_err(|_| "BEP 44 public key must be 32 bytes")?;
                    let signature: [u8; 64] = dict
                        .get(&b"sig"[..])
                        .and_then(BencodeValue::as_bytes)
                        .ok_or("BEP 44 mutable item missing signature")?
                        .try_into()
                        .map_err(|_| "BEP 44 signature must be 64 bytes")?;
                    let salt = match dict.get(&b"salt"[..]) {
                        Some(value) => Some(
                            value
                                .as_bytes()
                                .ok_or("BEP 44 mutable item salt must be bytes")?
                                .to_vec(),
                        ),
                        None => None,
                    };
                    let item = super::modern::MutableValue {
                        public_key,
                        signature,
                        sequence: dict
                            .get(&b"seq"[..])
                            .and_then(BencodeValue::as_int)
                            .ok_or("BEP 44 mutable item missing sequence")?,
                        salt,
                        value,
                    };
                    if StoredItem::mutable_target(&public_key, item.salt.as_deref()) != target
                        || item.sequence < 0
                        || !item.verify_signature()
                    {
                        return Err("BEP 44 mutable item validation failed".to_string());
                    }
                    store
                        .items
                        .write()
                        .map_err(|_| "BEP 44 item store lock poisoned".to_string())?
                        .insert(target, StoredItem::Mutable { target, item });
                }
                _ => return Err("BEP 44 item kind is invalid".to_string()),
            }
        }
        Ok(store)
    }

    pub fn save_to_file_sync(&self, path: &Path) -> Result<(), String> {
        let data = self.serialize()?;
        let temp_path = path.with_extension(format!("items.tmp{}", rand::random::<u32>()));
        std::fs::write(&temp_path, data)
            .map_err(|e| format!("write {}: {e}", temp_path.display()))?;
        #[cfg(windows)]
        let _ = std::fs::remove_file(path);
        std::fs::rename(&temp_path, path).map_err(|e| {
            let _ = std::fs::remove_file(&temp_path);
            format!("replace {}: {e}", path.display())
        })
    }

    pub fn load_from_file_sync(path: &Path) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Self::deserialize(&data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::BTreeMap;

    #[test]
    fn mutable_store_enforces_cas() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let store = DhtItemStore::default();
        let mut first = super::super::modern::MutableValue {
            public_key: key.verifying_key().to_bytes(),
            signature: [0u8; 64],
            sequence: 1,
            salt: None,
            value: BencodeValue::Bytes(b"one".to_vec()),
        };
        first.signature = key.sign(&first.signed_payload()).to_bytes();
        let target = store.put_mutable(first.clone(), None).unwrap();
        assert!(matches!(
            store.get(&target),
            Some(StoredItem::Mutable { .. })
        ));
        assert_eq!(
            store.put_mutable(first, Some(0)),
            Err(StoreError::SequenceTooLow)
        );
    }

    #[test]
    fn store_roundtrips_immutable_and_mutable_items() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let store = DhtItemStore::default();
        let immutable_target = store
            .put_immutable(BencodeValue::Bytes(b"immutable".to_vec()))
            .unwrap();
        let mut mutable = super::super::modern::MutableValue {
            public_key: key.verifying_key().to_bytes(),
            signature: [0u8; 64],
            sequence: 4,
            salt: Some(b"salt".to_vec()),
            value: BencodeValue::Bytes(b"mutable".to_vec()),
        };
        mutable.signature = key.sign(&mutable.signed_payload()).to_bytes();
        let mutable_target = store.put_mutable(mutable, None).unwrap();
        let restored = DhtItemStore::deserialize(&store.serialize().unwrap()).unwrap();
        assert_eq!(
            restored.get(&immutable_target),
            store.get(&immutable_target)
        );
        assert_eq!(restored.get(&mutable_target), store.get(&mutable_target));
    }

    #[test]
    fn deserialize_rejects_tampered_store_entry() {
        let store = DhtItemStore::default();
        store
            .put_immutable(BencodeValue::Bytes(b"protected".to_vec()))
            .unwrap();
        let mut encoded = store.serialize().unwrap();
        let position = encoded
            .iter()
            .position(|byte| *byte == b'p')
            .expect("serialized value should contain the test byte");
        encoded[position] = b'x';
        assert!(DhtItemStore::deserialize(&encoded).is_err());
    }

    #[test]
    fn deserialize_rejects_non_bytes_salt() {
        let key = SigningKey::from_bytes(&[8u8; 32]);
        let mut item = super::super::modern::MutableValue {
            public_key: key.verifying_key().to_bytes(),
            signature: [0u8; 64],
            sequence: 1,
            salt: None,
            value: BencodeValue::Bytes(b"value".to_vec()),
        };
        use ed25519_dalek::Signer;
        item.signature = key.sign(&item.signed_payload()).to_bytes();
        let target = StoredItem::mutable_target(&item.public_key, None);
        let mut entry = BTreeMap::new();
        entry.insert(b"kind".to_vec(), BencodeValue::Bytes(b"mutable".to_vec()));
        entry.insert(b"target".to_vec(), BencodeValue::Bytes(target.to_vec()));
        entry.insert(b"k".to_vec(), BencodeValue::Bytes(item.public_key.to_vec()));
        entry.insert(
            b"sig".to_vec(),
            BencodeValue::Bytes(item.signature.to_vec()),
        );
        entry.insert(b"seq".to_vec(), BencodeValue::Int(item.sequence));
        entry.insert(b"salt".to_vec(), BencodeValue::Int(1));
        entry.insert(b"v".to_vec(), item.value);
        let encoded = BencodeValue::List(vec![BencodeValue::Dict(entry)]).encode();
        assert!(DhtItemStore::deserialize(&encoded).is_err());
    }

    #[test]
    fn repeated_file_save_replaces_existing_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dht.items");
        let store = DhtItemStore::default();
        let first = store
            .put_immutable(BencodeValue::Bytes(b"first".to_vec()))
            .unwrap();
        store.save_to_file_sync(&path).unwrap();
        let second = store
            .put_immutable(BencodeValue::Bytes(b"second".to_vec()))
            .unwrap();
        store.save_to_file_sync(&path).unwrap();

        let restored = DhtItemStore::load_from_file_sync(&path).unwrap();
        assert!(restored.get(&first).is_some());
        assert!(restored.get(&second).is_some());
    }
}
