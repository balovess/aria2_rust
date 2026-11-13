//! In-memory BEP 44 item store.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, RwLock};

use super::modern::{MutableUpdate, StoredItem, accept_mutable_update};
use crate::bittorrent::bencode::codec::BencodeValue;

const MAX_VALUE_BYTES: usize = 1000;
const MAX_SALT_BYTES: usize = 64;
pub const DEFAULT_MAX_ITEMS: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    ValueTooLarge,
    SaltTooLarge,
    InvalidTarget,
    InvalidSignature,
    SequenceTooLow,
    CasMismatch,
}

struct StoredEntry {
    item: StoredItem,
    order: u64,
}

struct ItemStore {
    items: HashMap<[u8; 20], StoredEntry>,
    least_recent: BTreeSet<(u64, [u8; 20])>,
    next_order: u64,
    max_items: usize,
}

impl ItemStore {
    fn new(max_items: usize) -> Self {
        Self {
            items: HashMap::new(),
            least_recent: BTreeSet::new(),
            next_order: 0,
            max_items: max_items.max(1),
        }
    }

    fn insert(&mut self, target: [u8; 20], item: StoredItem) {
        if let Some(previous) = self.items.remove(&target) {
            self.least_recent.remove(&(previous.order, target));
        } else if self.items.len() >= self.max_items
            && let Some((_, oldest_target)) = self.least_recent.pop_first()
        {
            self.items.remove(&oldest_target);
        }

        let order = self.next_order;
        self.next_order = self.next_order.wrapping_add(1);
        self.items.insert(target, StoredEntry { item, order });
        self.least_recent.insert((order, target));
    }

    fn set_limit(&mut self, max_items: usize) {
        self.max_items = max_items.max(1);
        while self.items.len() > self.max_items {
            let Some((_, target)) = self.least_recent.pop_first() else {
                break;
            };
            self.items.remove(&target);
        }
    }
}

#[derive(Clone)]
pub struct DhtItemStore {
    items: Arc<RwLock<ItemStore>>,
}

impl Default for DhtItemStore {
    fn default() -> Self {
        Self::with_max_items(DEFAULT_MAX_ITEMS)
    }
}

impl DhtItemStore {
    /// Create a store with a bounded number of retained BEP 44 targets.
    ///
    /// Zero is normalized to one. A new target at capacity evicts the oldest
    /// stored target; updating an existing target makes it the newest.
    pub fn with_max_items(max_items: usize) -> Self {
        Self {
            items: Arc::new(RwLock::new(ItemStore::new(max_items))),
        }
    }

    /// Apply a new limit immediately, evicting the oldest entries as needed.
    pub fn set_max_items(&self, max_items: usize) {
        if let Ok(mut items) = self.items.write() {
            items.set_limit(max_items);
        }
    }

    pub fn len(&self) -> usize {
        self.items
            .read()
            .map(|items| items.items.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn max_items(&self) -> usize {
        self.items.read().map(|items| items.max_items).unwrap_or(1)
    }

    pub fn targets(&self) -> Vec<[u8; 20]> {
        self.items
            .read()
            .map(|items| items.items.keys().copied().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, target: &[u8; 20]) -> Option<StoredItem> {
        self.items
            .read()
            .ok()?
            .items
            .get(target)
            .map(|entry| entry.item.clone())
    }

    pub fn put_immutable(&self, value: BencodeValue) -> Result<[u8; 20], StoreError> {
        if value_exceeds_limit(&value) {
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
        if value_exceeds_limit(&item.value) {
            return Err(StoreError::ValueTooLarge);
        }
        if item.salt.as_deref().is_some_and(salt_exceeds_limit) {
            return Err(StoreError::SaltTooLarge);
        }
        if item.sequence < 0 || !item.verify_signature() {
            return Err(StoreError::InvalidSignature);
        }
        let target = StoredItem::mutable_target(&item.public_key, item.salt.as_deref());
        let mut items = self.items.write().map_err(|_| StoreError::InvalidTarget)?;
        let current = items
            .items
            .get(&target)
            .and_then(|stored| match &stored.item {
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
            .least_recent
            .iter()
            .filter_map(|(_, target)| items.items.get(target))
            .map(|stored| {
                let mut entry = std::collections::BTreeMap::new();
                match &stored.item {
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
        Self::deserialize_with_limit(data, DEFAULT_MAX_ITEMS)
    }

    /// Restore persisted entries using the configured capacity. Older entries
    /// are discarded first if the file contains more items than the limit.
    pub fn deserialize_with_limit(data: &[u8], max_items: usize) -> Result<Self, String> {
        let (value, consumed) = BencodeValue::decode(data)?;
        if consumed != data.len() {
            return Err("BEP 44 item store has trailing data".to_string());
        }
        let entries = value
            .as_list()
            .ok_or("BEP 44 item store root must be a list")?;
        let store = Self::with_max_items(max_items);
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
            if value_exceeds_limit(&value) {
                return Err("BEP 44 item value exceeds 1000 encoded bytes".to_string());
            }
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
                    if salt.as_deref().is_some_and(salt_exceeds_limit) {
                        return Err("BEP 44 item salt exceeds 64 bytes".to_string());
                    }
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
        super::persistence::write_atomic_file_sync(path, &data)
    }

    pub fn load_from_file_sync(path: &Path) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Self::deserialize(&data)
    }

    /// Read and validate the persisted store without blocking the async
    /// runtime. Signature verification and bencode decoding run on the
    /// blocking pool because a store can contain many entries.
    pub async fn load_from_file(path: &Path) -> Result<Self, String> {
        Self::load_from_file_with_limit(path, DEFAULT_MAX_ITEMS).await
    }

    pub async fn load_from_file_with_limit(path: &Path, max_items: usize) -> Result<Self, String> {
        let data = tokio::fs::read(path)
            .await
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        tokio::task::spawn_blocking(move || Self::deserialize_with_limit(&data, max_items))
            .await
            .map_err(|error| format!("parse DHT item store task failed: {error}"))?
    }
}

fn value_exceeds_limit(value: &BencodeValue) -> bool {
    value.encode().len() > MAX_VALUE_BYTES
}

fn salt_exceeds_limit(salt: &[u8]) -> bool {
    salt.len() > MAX_SALT_BYTES
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
    fn capacity_evicts_oldest_target_and_persists_recency_order() {
        let store = DhtItemStore::with_max_items(2);
        let first = store
            .put_immutable(BencodeValue::Bytes(b"first".to_vec()))
            .unwrap();
        let second = store
            .put_immutable(BencodeValue::Bytes(b"second".to_vec()))
            .unwrap();
        store
            .put_immutable(BencodeValue::Bytes(b"first".to_vec()))
            .unwrap();
        let third = store
            .put_immutable(BencodeValue::Bytes(b"third".to_vec()))
            .unwrap();

        assert_eq!(store.len(), 2);
        assert!(store.get(&first).is_some());
        assert!(store.get(&second).is_none());
        assert!(store.get(&third).is_some());

        let restored = DhtItemStore::deserialize_with_limit(&store.serialize().unwrap(), 1)
            .expect("bounded item store should restore");
        assert_eq!(restored.len(), 1);
        assert!(restored.get(&third).is_some());
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
    fn deserialize_rejects_oversized_immutable_value() {
        let value = BencodeValue::Bytes(vec![b'x'; MAX_VALUE_BYTES]);
        let target = StoredItem::immutable_target(&value);
        let entry = BTreeMap::from([
            (b"kind".to_vec(), BencodeValue::Bytes(b"immutable".to_vec())),
            (b"target".to_vec(), BencodeValue::Bytes(target.to_vec())),
            (b"v".to_vec(), value),
        ]);
        let encoded = BencodeValue::List(vec![BencodeValue::Dict(entry)]).encode();

        assert_eq!(
            DhtItemStore::deserialize(&encoded)
                .err()
                .expect("oversized immutable item must be rejected"),
            "BEP 44 item value exceeds 1000 encoded bytes"
        );
    }

    #[test]
    fn deserialize_rejects_oversized_mutable_salt() {
        let public_key = [7u8; 32];
        let salt = vec![b's'; MAX_SALT_BYTES + 1];
        let target = StoredItem::mutable_target(&public_key, Some(&salt));
        let entry = BTreeMap::from([
            (b"kind".to_vec(), BencodeValue::Bytes(b"mutable".to_vec())),
            (b"target".to_vec(), BencodeValue::Bytes(target.to_vec())),
            (b"k".to_vec(), BencodeValue::Bytes(public_key.to_vec())),
            (b"sig".to_vec(), BencodeValue::Bytes(vec![0; 64])),
            (b"seq".to_vec(), BencodeValue::Int(1)),
            (b"salt".to_vec(), BencodeValue::Bytes(salt)),
            (b"v".to_vec(), BencodeValue::Bytes(b"small value".to_vec())),
        ]);
        let encoded = BencodeValue::List(vec![BencodeValue::Dict(entry)]).encode();

        assert_eq!(
            DhtItemStore::deserialize(&encoded)
                .err()
                .expect("oversized mutable salt must be rejected"),
            "BEP 44 item salt exceeds 64 bytes"
        );
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
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
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

    #[tokio::test]
    async fn async_file_load_roundtrips_the_store() {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let path = dir.path().join("dht.items");
        let store = DhtItemStore::default();
        let target = store
            .put_immutable(BencodeValue::Bytes(b"async item".to_vec()))
            .unwrap();
        store.save_to_file_sync(&path).unwrap();

        let restored = DhtItemStore::load_from_file(&path)
            .await
            .expect("async store load should succeed");

        assert_eq!(restored.get(&target), store.get(&target));
    }
}
