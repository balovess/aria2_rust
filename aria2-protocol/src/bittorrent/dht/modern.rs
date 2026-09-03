//! Structured payloads for BEP 44 and BEP 51.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha1::{Digest, Sha1};

use super::message::{DhtMessage, DhtQueryMethod};
use crate::bittorrent::bencode::codec::BencodeValue;

#[derive(Debug, Clone, PartialEq)]
pub struct MutableValue {
    pub public_key: [u8; 32],
    pub signature: [u8; 64],
    pub sequence: i64,
    pub salt: Option<Vec<u8>>,
    pub value: BencodeValue,
}

impl MutableValue {
    /// Build the exact bencoded value covered by a BEP 44 mutable signature.
    pub fn signed_payload(&self) -> Vec<u8> {
        // BEP 44 signs the concatenation below, rather than a dictionary
        // containing pk/salt. This exact byte sequence is interoperability
        // critical because the value may itself be any bencode type.
        let mut payload = Vec::new();
        if let Some(salt) = self.salt.as_deref() {
            payload.extend_from_slice(format!("4:salt{}:", salt.len()).as_bytes());
            payload.extend_from_slice(salt);
        }
        payload.extend_from_slice(b"3:seqi");
        payload.extend_from_slice(self.sequence.to_string().as_bytes());
        payload.extend_from_slice(b"e1:v");
        payload.extend_from_slice(&self.value.encode());
        payload
    }

    pub fn verify_signature(&self) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(&self.public_key) else {
            return false;
        };
        let signature = Signature::from_bytes(&self.signature);
        key.verify(&self.signed_payload(), &signature).is_ok()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoredItem {
    Immutable {
        target: [u8; 20],
        value: BencodeValue,
    },
    Mutable {
        target: [u8; 20],
        item: MutableValue,
    },
}

impl StoredItem {
    pub fn immutable_target(value: &BencodeValue) -> [u8; 20] {
        Sha1::digest(value.encode()).into()
    }

    pub fn mutable_target(public_key: &[u8; 32], salt: Option<&[u8]>) -> [u8; 20] {
        let salt_len = salt.map_or(0, <[u8]>::len);
        let mut input = Vec::with_capacity(32 + salt_len);
        input.extend_from_slice(public_key);
        if let Some(salt) = salt {
            input.extend_from_slice(salt);
        }
        Sha1::digest(input).into()
    }

    pub fn verify_target(&self) -> bool {
        match self {
            Self::Immutable { target, value } => {
                value.encode().len() <= 1000 && target == &Self::immutable_target(value)
            }
            Self::Mutable { target, item } => {
                item.sequence >= 0
                    && item.value.encode().len() <= 1000
                    && item.salt.as_deref().is_none_or(|salt| salt.len() <= 64)
                    && target == &Self::mutable_target(&item.public_key, item.salt.as_deref())
                    && item.verify_signature()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutableUpdate {
    Accepted,
    RejectedSequence,
    RejectedCas,
    RejectedSignature,
}

/// Apply a mutable BEP 44 update to an existing item.
pub fn accept_mutable_update(
    current: Option<&MutableValue>,
    candidate: &MutableValue,
    cas: Option<i64>,
) -> MutableUpdate {
    if candidate.sequence < 0 || candidate.value.encode().len() > 1000 {
        return MutableUpdate::RejectedSignature;
    }
    if candidate
        .salt
        .as_deref()
        .is_some_and(|salt| salt.len() > 64)
        || !candidate.verify_signature()
    {
        return MutableUpdate::RejectedSignature;
    }
    if let Some(current) = current {
        if candidate.sequence <= current.sequence {
            return MutableUpdate::RejectedSequence;
        }
        if cas.is_some_and(|expected| expected != current.sequence) {
            return MutableUpdate::RejectedCas;
        }
    } else if cas.is_some() {
        return MutableUpdate::RejectedCas;
    }
    MutableUpdate::Accepted
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleInfoHashesResponse {
    pub interval: i64,
    pub num: i64,
    pub samples: Vec<[u8; 20]>,
    pub nodes: Vec<u8>,
    pub nodes6: Vec<u8>,
}

impl SampleInfoHashesResponse {
    pub fn from_bencode(value: &BencodeValue) -> Result<Self, String> {
        let interval = value
            .dict_get(b"interval")
            .and_then(|v| v.as_int())
            .ok_or("sample_infohashes response missing interval")?;
        if interval <= 0 {
            return Err("sample_infohashes interval must be positive".to_string());
        }
        let num = value
            .dict_get(b"num")
            .and_then(|v| v.as_int())
            .ok_or("sample_infohashes response missing num")?;
        if num < 0 {
            return Err("sample_infohashes num must be non-negative".to_string());
        }
        let raw_samples = value
            .dict_get(b"samples")
            .and_then(|v| v.as_bytes())
            .ok_or("sample_infohashes response missing samples")?;
        if raw_samples.len() % 20 != 0 {
            return Err("sample_infohashes samples must be 20-byte hashes".to_string());
        }
        let samples: Vec<[u8; 20]> = raw_samples
            .as_chunks::<20>()
            .0
            .iter()
            .map(|chunk| {
                let mut hash = [0u8; 20];
                hash.copy_from_slice(chunk);
                hash
            })
            .collect();
        let num_usize = usize::try_from(num)
            .map_err(|_| "sample_infohashes num does not fit platform size".to_string())?;
        if samples.len() > num_usize {
            return Err("sample_infohashes has more samples than num".to_string());
        }
        let nodes = value
            .dict_get(b"nodes")
            .map(|v| {
                v.as_bytes()
                    .ok_or("sample_infohashes nodes must be a byte string")
                    .map(ToOwned::to_owned)
            })
            .transpose()?
            .unwrap_or_default();
        let nodes6 = value
            .dict_get(b"nodes6")
            .map(|v| {
                v.as_bytes()
                    .ok_or("sample_infohashes nodes6 must be a byte string")
                    .map(ToOwned::to_owned)
            })
            .transpose()?
            .unwrap_or_default();
        if !nodes.len().is_multiple_of(26) || !nodes6.len().is_multiple_of(38) {
            return Err("sample_infohashes compact nodes have invalid length".to_string());
        }
        Ok(Self {
            interval,
            num,
            samples,
            nodes,
            nodes6,
        })
    }
}

pub fn sample_infohashes_query(
    transaction_id: u32,
    sender_id: &[u8; 20],
    target: &[u8; 20],
) -> DhtMessage {
    let mut args = BTreeMap::new();
    args.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
    args.insert(b"target".to_vec(), BencodeValue::Bytes(target.to_vec()));
    DhtMessage::new_query(
        transaction_id,
        DhtQueryMethod::SAMPLE_INFOHASHES,
        BencodeValue::Dict(args),
    )
}

pub fn get_query(
    transaction_id: u32,
    sender_id: &[u8; 20],
    target: &[u8; 20],
    seq: Option<i64>,
) -> DhtMessage {
    let mut args = BTreeMap::new();
    args.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
    args.insert(b"target".to_vec(), BencodeValue::Bytes(target.to_vec()));
    if let Some(seq) = seq {
        args.insert(b"seq".to_vec(), BencodeValue::Int(seq));
    }
    DhtMessage::new_query(
        transaction_id,
        DhtQueryMethod::GET,
        BencodeValue::Dict(args),
    )
}

pub fn put_query(
    transaction_id: u32,
    sender_id: &[u8; 20],
    token: &[u8],
    item: &MutableValue,
    cas: Option<i64>,
) -> DhtMessage {
    let mut args = BTreeMap::new();
    args.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
    args.insert(b"token".to_vec(), BencodeValue::Bytes(token.to_vec()));
    args.insert(b"v".to_vec(), item.value.clone());
    args.insert(b"k".to_vec(), BencodeValue::Bytes(item.public_key.to_vec()));
    args.insert(
        b"sig".to_vec(),
        BencodeValue::Bytes(item.signature.to_vec()),
    );
    args.insert(b"seq".to_vec(), BencodeValue::Int(item.sequence));
    if let Some(salt) = item.salt.as_deref() {
        args.insert(b"salt".to_vec(), BencodeValue::Bytes(salt.to_vec()));
    }
    if let Some(cas) = cas {
        args.insert(b"cas".to_vec(), BencodeValue::Int(cas));
    }
    DhtMessage::new_query(
        transaction_id,
        DhtQueryMethod::PUT,
        BencodeValue::Dict(args),
    )
}

pub fn put_immutable_query(
    transaction_id: u32,
    sender_id: &[u8; 20],
    token: &[u8],
    value: &BencodeValue,
) -> DhtMessage {
    let mut args = BTreeMap::new();
    args.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
    args.insert(b"token".to_vec(), BencodeValue::Bytes(token.to_vec()));
    args.insert(b"v".to_vec(), value.clone());
    DhtMessage::new_query(
        transaction_id,
        DhtQueryMethod::PUT,
        BencodeValue::Dict(args),
    )
}

pub fn sample_infohashes_response(
    transaction_id: &[u8],
    sender_id: &[u8; 20],
    response: &SampleInfoHashesResponse,
) -> DhtMessage {
    let samples = response
        .samples
        .iter()
        .flat_map(|hash| hash.iter().copied())
        .collect();
    let mut result = BTreeMap::new();
    result.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
    result.insert(b"interval".to_vec(), BencodeValue::Int(response.interval));
    result.insert(b"num".to_vec(), BencodeValue::Int(response.num));
    result.insert(b"samples".to_vec(), BencodeValue::Bytes(samples));
    if !response.nodes.is_empty() {
        result.insert(
            b"nodes".to_vec(),
            BencodeValue::Bytes(response.nodes.clone()),
        );
    }
    if !response.nodes6.is_empty() {
        result.insert(
            b"nodes6".to_vec(),
            BencodeValue::Bytes(response.nodes6.clone()),
        );
    }
    DhtMessage::new_response(transaction_id.to_vec(), BencodeValue::Dict(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_target_is_content_addressed() {
        let value = BencodeValue::Bytes(b"value".to_vec());
        let item = StoredItem::Immutable {
            target: StoredItem::immutable_target(&value),
            value,
        };
        assert!(item.verify_target());
    }

    #[test]
    fn mutable_updates_require_valid_signature_and_monotonic_sequence() {
        use ed25519_dalek::{Signer, SigningKey};

        let signing_key = SigningKey::from_bytes(&[3u8; 32]);
        let mut item = MutableValue {
            public_key: signing_key.verifying_key().to_bytes(),
            signature: [0u8; 64],
            sequence: 1,
            salt: Some(b"salt".to_vec()),
            value: BencodeValue::Bytes(b"v1".to_vec()),
        };
        item.signature = signing_key.sign(&item.signed_payload()).to_bytes();
        assert!(item.verify_signature());
        assert_eq!(
            accept_mutable_update(None, &item, None),
            MutableUpdate::Accepted
        );

        let mut next = item.clone();
        next.sequence = 2;
        next.value = BencodeValue::Bytes(b"v2".to_vec());
        next.signature = signing_key.sign(&next.signed_payload()).to_bytes();
        assert_eq!(
            accept_mutable_update(Some(&item), &next, Some(1)),
            MutableUpdate::Accepted
        );
        assert_eq!(
            accept_mutable_update(Some(&item), &next, Some(0)),
            MutableUpdate::RejectedCas
        );
    }

    #[test]
    fn bep44_mutable_test_vector_without_salt() {
        let public_key: [u8; 32] =
            hex::decode("77ff84905a91936367c01360803104f92432fcd904a43511876df5cdf3e7e548")
                .unwrap()
                .try_into()
                .unwrap();
        let signature: [u8; 64] = hex::decode(
            "305ac8aeb6c9c151fa120f120ea2cfb923564e11552d06a5d856091e5e853cff1260d3f39e4999684aa92eb73ffd136e6f4f3ecbfda0ce53a1608ecd7ae21f01",
        )
        .unwrap()
        .try_into()
        .unwrap();
        let item = MutableValue {
            public_key,
            signature,
            sequence: 1,
            salt: None,
            value: BencodeValue::Bytes(b"Hello World!".to_vec()),
        };
        assert_eq!(item.signed_payload(), b"3:seqi1e1:v12:Hello World!");
        assert_eq!(
            hex::encode(StoredItem::mutable_target(&public_key, None)),
            "4a533d47ec9c7d95b1ad75f576cffc641853b750"
        );
        assert!(item.verify_signature());
    }

    #[test]
    fn sample_infohashes_roundtrip_and_rejects_bad_samples() {
        let response = SampleInfoHashesResponse {
            interval: 900,
            num: 1,
            samples: vec![[7u8; 20]],
            nodes: vec![0u8; 26],
            nodes6: vec![0u8; 38],
        };
        let message = sample_infohashes_response(b"tx", &[1u8; 20], &response);
        let decoded = DhtMessage::decode(&message.encode().unwrap()).unwrap();
        let parsed = SampleInfoHashesResponse::from_bencode(decoded.r.as_ref().unwrap()).unwrap();
        assert_eq!(parsed, response);

        let mut bad = BTreeMap::new();
        bad.insert(b"interval".to_vec(), BencodeValue::Int(1));
        bad.insert(b"num".to_vec(), BencodeValue::Int(1));
        bad.insert(b"samples".to_vec(), BencodeValue::Bytes(vec![0u8; 19]));
        assert!(SampleInfoHashesResponse::from_bencode(&BencodeValue::Dict(bad)).is_err());

        let mut bad_interval = BTreeMap::new();
        bad_interval.insert(b"interval".to_vec(), BencodeValue::Int(0));
        bad_interval.insert(b"num".to_vec(), BencodeValue::Int(0));
        bad_interval.insert(b"samples".to_vec(), BencodeValue::Bytes(Vec::new()));
        assert!(SampleInfoHashesResponse::from_bencode(&BencodeValue::Dict(bad_interval)).is_err());

        let mut bad_count = BTreeMap::new();
        bad_count.insert(b"interval".to_vec(), BencodeValue::Int(1));
        bad_count.insert(b"num".to_vec(), BencodeValue::Int(0));
        bad_count.insert(b"samples".to_vec(), BencodeValue::Bytes(vec![0u8; 20]));
        assert!(SampleInfoHashesResponse::from_bencode(&BencodeValue::Dict(bad_count)).is_err());

        let mut bad_nodes = BTreeMap::new();
        bad_nodes.insert(b"interval".to_vec(), BencodeValue::Int(1));
        bad_nodes.insert(b"num".to_vec(), BencodeValue::Int(0));
        bad_nodes.insert(b"samples".to_vec(), BencodeValue::Bytes(Vec::new()));
        bad_nodes.insert(b"nodes".to_vec(), BencodeValue::Int(1));
        assert!(SampleInfoHashesResponse::from_bencode(&BencodeValue::Dict(bad_nodes)).is_err());

        let mut bad_nodes6 = BTreeMap::new();
        bad_nodes6.insert(b"interval".to_vec(), BencodeValue::Int(1));
        bad_nodes6.insert(b"num".to_vec(), BencodeValue::Int(0));
        bad_nodes6.insert(b"samples".to_vec(), BencodeValue::Bytes(Vec::new()));
        bad_nodes6.insert(b"nodes6".to_vec(), BencodeValue::Int(1));
        assert!(SampleInfoHashesResponse::from_bencode(&BencodeValue::Dict(bad_nodes6)).is_err());
    }
}
