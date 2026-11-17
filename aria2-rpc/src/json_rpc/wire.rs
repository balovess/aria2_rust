use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::borrow::Cow;
use std::fmt;

use super::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

/// One item in the original aria2 JSON-RPC wire envelope.
///
/// The C++ parser handles object-level failures while iterating a batch, so a
/// batch can contain both executable requests and already-materialized error
/// responses. Keeping that distinction at the wire seam prevents malformed
/// entries from reaching the engine dispatcher.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcWireEntry {
    Request(JsonRpcRequest),
    Error(JsonRpcResponse),
}

/// Parsed JSON-RPC document using aria2_original's envelope semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonRpcWireDocument {
    /// Whether the input was a JSON array. A one-item batch is still encoded
    /// as an array in the response.
    pub is_batch: bool,
    pub entries: Vec<JsonRpcWireEntry>,
}

/// Parse one object directly from serde's map access.
///
/// The previous implementation first materialized the entire JSON document
/// as a `Value`, then cloned `id`, `method`, and `params` out of that DOM. The
/// wire parser only needs a few owned fields, so consuming the map avoids the
/// second tree walk and the associated clones while preserving aria2's
/// object-level error responses.
fn parse_aria2_wire_object<'de, A>(mut object: A) -> Result<JsonRpcWireEntry, A::Error>
where
    A: MapAccess<'de>,
{
    let mut id: Option<Option<serde_json::Value>> = None;
    let mut method = None;
    let mut params = None;
    let mut version = None;

    while let Some(key) = object.next_key::<Cow<'de, str>>()? {
        match key.as_ref() {
            "id" => id = Some(object.next_value::<Option<serde_json::Value>>()?),
            "method" => method = Some(object.next_value::<serde_json::Value>()?),
            "params" => params = Some(object.next_value::<serde_json::Value>()?),
            "jsonrpc" => version = Some(object.next_value::<serde_json::Value>()?),
            _ => {
                let _: IgnoredAny = object.next_value()?;
            }
        }
    }

    let id = match id {
        Some(Some(id)) => id,
        Some(None) => serde_json::Value::Null,
        None => {
            return Ok(JsonRpcWireEntry::Error(
                JsonRpcError::InvalidRequest("Invalid Request.".to_string()).into_response(None),
            ));
        }
    };

    let Some(method) = method.and_then(|value| value.as_str().map(str::to_owned)) else {
        return Ok(JsonRpcWireEntry::Error(
            JsonRpcError::InvalidRequest("Invalid Request.".to_string()).into_response(Some(id)),
        ));
    };

    // aria2_original ignores the jsonrpc member and treats an omitted params
    // member as an empty positional list. Named/object params are rejected by
    // rpc_helper.cc before a method is executed.
    let params = match params {
        None => serde_json::Value::Array(Vec::new()),
        Some(serde_json::Value::Array(params)) => serde_json::Value::Array(params),
        Some(_) => {
            return Ok(JsonRpcWireEntry::Error(
                JsonRpcError::InvalidParams("Invalid params.".to_string()).into_response(Some(id)),
            ));
        }
    };

    Ok(JsonRpcWireEntry::Request(JsonRpcRequest {
        version: version.and_then(|value| value.as_str().map(str::to_owned)),
        method,
        params,
        id: Some(id),
    }))
}

enum WireBatchItem {
    Entry(JsonRpcWireEntry),
    Ignored,
}

struct WireBatchItemVisitor;

impl<'de> Visitor<'de> for WireBatchItemVisitor {
    type Value = WireBatchItem;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON-RPC object or an ignored JSON value")
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        parse_aria2_wire_object(map).map(WireBatchItem::Entry)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(WireBatchItem::Ignored)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(WireBatchItem::Ignored)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(WireBatchItem::Ignored)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(WireBatchItem::Ignored)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(WireBatchItem::Ignored)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(WireBatchItem::Ignored)
    }

    fn visit_bytes<E>(self, _: &[u8]) -> Result<Self::Value, E> {
        Ok(WireBatchItem::Ignored)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(WireBatchItem::Ignored)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(WireBatchItem::Ignored)
    }
}

impl<'de> Deserialize<'de> for WireBatchItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(WireBatchItemVisitor)
    }
}

enum WireRoot {
    Single(JsonRpcWireEntry),
    Batch(Vec<JsonRpcWireEntry>),
    Invalid,
}

struct WireRootVisitor;

impl<'de> Visitor<'de> for WireRootVisitor {
    type Value = WireRoot;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON-RPC object or batch array")
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        parse_aria2_wire_object(map).map(WireRoot::Single)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut entries = Vec::new();
        while let Some(item) = seq.next_element::<WireBatchItem>()? {
            if let WireBatchItem::Entry(entry) = item {
                entries.push(entry);
            }
        }
        Ok(WireRoot::Batch(entries))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(WireRoot::Invalid)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(WireRoot::Invalid)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(WireRoot::Invalid)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(WireRoot::Invalid)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(WireRoot::Invalid)
    }

    fn visit_bytes<E>(self, _: &[u8]) -> Result<Self::Value, E> {
        Ok(WireRoot::Invalid)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(WireRoot::Invalid)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(WireRoot::Invalid)
    }
}

/// Parse a JSON-RPC document with the externally observable aria2 semantics.
///
/// This adapter intentionally differs from the general-purpose
/// [`parse_request`] helper used by typed Rust callers. `aria2_original`:
///
/// - does not validate the `jsonrpc` member;
/// - defaults an omitted `params` member to an empty list;
/// - requires an `id` before dispatching an object request;
/// - returns object-level errors inside a batch;
/// - ignores non-object elements in a batch; and
/// - returns an empty array for an empty batch.
pub fn parse_aria2_wire_document(data: &[u8]) -> Result<JsonRpcWireDocument, JsonRpcError> {
    let mut deserializer = serde_json::Deserializer::from_slice(data);
    let root = Deserializer::deserialize_any(&mut deserializer, WireRootVisitor)
        .map_err(|_| JsonRpcError::ParseError("Parse error.".to_string()))?;
    deserializer
        .end()
        .map_err(|_| JsonRpcError::ParseError("Parse error.".to_string()))?;

    match root {
        WireRoot::Single(entry) => Ok(JsonRpcWireDocument {
            is_batch: false,
            entries: vec![entry],
        }),
        WireRoot::Batch(entries) => Ok(JsonRpcWireDocument {
            is_batch: true,
            entries,
        }),
        WireRoot::Invalid => Err(JsonRpcError::InvalidRequest("Invalid Request.".to_string())),
    }
}
