use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcError {
    ParseError(String),
    InvalidRequest(String),
    MethodNotFound(String),
    InvalidParams(String),
    InternalError(String),
    ServerError(i32, String),
    /// Domain error (e.g. "GID not found", "No such method") — code: 1 (matches C++).
    RpcExecution(String),
    /// Authentication failure; aria2 reports this as execution error code 1.
    Unauthorized(String),
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParseError(s) => write!(f, "Parse error: {}", s),
            Self::InvalidRequest(s) => write!(f, "Invalid Request: {}", s),
            Self::MethodNotFound(s) => write!(f, "Method not found: {}", s),
            Self::InvalidParams(s) => write!(f, "Invalid params: {}", s),
            Self::InternalError(s) => write!(f, "Internal error: {}", s),
            Self::ServerError(code, s) => write!(f, "Server error ({}): {}", code, s),
            Self::RpcExecution(s) => write!(f, "Error: {}", s),
            Self::Unauthorized(s) => write!(f, "Unauthorized: {}", s),
        }
    }
}

impl std::error::Error for JsonRpcError {}

impl JsonRpcError {
    pub fn code(&self) -> i32 {
        match self {
            Self::ParseError(_) => -32700,
            Self::InvalidRequest(_) => -32600,
            Self::MethodNotFound(_) => -32601,
            Self::InvalidParams(_) => -32602,
            Self::InternalError(_) => -32603,
            Self::ServerError(c, _) => *c,
            Self::RpcExecution(_) => 1,
            Self::Unauthorized(_) => 1,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::ParseError(s)
            | Self::InvalidRequest(s)
            | Self::MethodNotFound(s)
            | Self::InvalidParams(s)
            | Self::InternalError(s)
            | Self::RpcExecution(s) => s.clone(),
            // C++ aria2 throws `DL_ABORT_EX("Unauthorized")` for both a
            // missing and an invalid rpc-secret. Keep diagnostic details
            // internal so the wire message remains compatible.
            Self::Unauthorized(_) => "Unauthorized".to_string(),
            Self::ServerError(_, s) => s.clone(),
        }
    }

    pub fn into_response(self, id: Option<serde_json::Value>) -> JsonRpcResponse {
        let error = RpcErrorResponse {
            code: self.code(),
            message: self.message(),
            data: None,
        };
        JsonRpcResponse {
            version: JSONRPC_VERSION.to_string(),
            id: id.unwrap_or(serde_json::Value::Null),
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcRequest {
    #[serde(rename = "jsonrpc")]
    pub version: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    pub id: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            version: Some(JSONRPC_VERSION.to_string()),
            method: method.into(),
            params,
            id: None,
        }
    }

    pub fn with_id(mut self, id: impl Into<serde_json::Value>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    pub fn validate(&self) -> Result<(), JsonRpcError> {
        if self.method.is_empty() {
            return Err(JsonRpcError::InvalidRequest(
                "method is required".to_string(),
            ));
        }
        if let Some(ref v) = self.version
            && v != JSONRPC_VERSION
        {
            return Err(JsonRpcError::InvalidRequest(format!(
                "unsupported jsonrpc version: {}",
                v
            )));
        }
        Ok(())
    }

    pub fn get_param<T: serde::de::DeserializeOwned>(
        &self,
        index: usize,
    ) -> Result<T, JsonRpcError> {
        match &self.params {
            serde_json::Value::Array(arr) if index < arr.len() => {
                serde_json::from_value(arr[index].clone()).map_err(|e| {
                    JsonRpcError::InvalidParams(format!("param[{}] type error: {}", index, e))
                })
            }
            serde_json::Value::Object(map) => {
                let key = format!("p{}", index);
                map.get(&key)
                    .ok_or_else(|| JsonRpcError::InvalidParams(format!("param[{}] missing", index)))
                    .and_then(|v| {
                        serde_json::from_value(v.clone()).map_err(|e| {
                            JsonRpcError::InvalidParams(format!(
                                "param[{}] type error: {}",
                                index, e
                            ))
                        })
                    })
            }
            _ => Err(JsonRpcError::InvalidParams(format!(
                "param[{}] not found",
                index
            ))),
        }
    }

    /// Return a positional parameter without changing its wire type.
    ///
    /// RPC handlers use this only when an optional parameter has multiple
    /// valid positions. The value is borrowed so a handler can distinguish a
    /// missing argument from a present argument with the wrong type without
    /// silently accepting an extension-shaped request.
    pub(crate) fn optional_param_value(&self, index: usize) -> Option<&serde_json::Value> {
        match &self.params {
            serde_json::Value::Array(params) => params.get(index),
            serde_json::Value::Object(params) => params.get(&format!("p{index}")),
            _ => None,
        }
    }

    /// Read a positional or named parameter when it is present.
    ///
    /// Unlike [`Self::get_param_or_default`], a present value is still
    /// type-checked. This preserves aria2's distinction between an omitted
    /// optional argument and a supplied argument with an invalid type.
    pub fn get_optional_param<T: serde::de::DeserializeOwned>(
        &self,
        index: usize,
    ) -> Result<Option<T>, JsonRpcError> {
        self.optional_param_value(index)
            .map(|value| {
                serde_json::from_value(value.clone()).map_err(|error| {
                    JsonRpcError::InvalidParams(format!("param[{}] type error: {}", index, error))
                })
            })
            .transpose()
    }

    pub fn get_param_or_default<T: serde::de::DeserializeOwned + Default>(
        &self,
        index: usize,
    ) -> T {
        self.get_param::<T>(index).unwrap_or_default()
    }

    pub fn get_param_by_name<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
    ) -> Result<T, JsonRpcError> {
        match &self.params {
            serde_json::Value::Object(map) => map
                .get(name)
                .ok_or_else(|| JsonRpcError::InvalidParams(format!("param '{}' missing", name)))
                .and_then(|v| {
                    serde_json::from_value(v.clone()).map_err(|e| {
                        JsonRpcError::InvalidParams(format!("param '{}' type error: {}", name, e))
                    })
                }),
            _ => Err(JsonRpcError::InvalidParams(
                "params must be an object for named parameters".to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcErrorResponse {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcResponse {
    #[serde(rename = "jsonrpc")]
    pub version: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcErrorResponse>,
}

impl JsonRpcResponse {
    pub fn success(id: impl Into<serde_json::Value>, result: impl Into<serde_json::Value>) -> Self {
        Self {
            version: JSONRPC_VERSION.to_string(),
            id: id.into(),
            result: Some(result.into()),
            error: None,
        }
    }

    pub fn error(id: impl Into<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            version: JSONRPC_VERSION.to_string(),
            id: id.into(),
            result: None,
            error: Some(RpcErrorResponse {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
    pub fn is_success(&self) -> bool {
        !self.is_error()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn to_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

pub struct JsonRpcBatchResponse(pub Vec<JsonRpcResponse>);

impl JsonRpcBatchResponse {
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.0)
    }

    pub fn to_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.0)
    }
}

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

pub fn parse_request(data: &[u8]) -> Result<Vec<JsonRpcRequest>, JsonRpcError> {
    let parsed: serde_json::Value =
        serde_json::from_slice(data).map_err(|e| JsonRpcError::ParseError(e.to_string()))?;

    match parsed {
        serde_json::Value::Object(object) => {
            let req = serde_json::from_value::<JsonRpcRequest>(serde_json::Value::Object(object))
                .map_err(|e| JsonRpcError::InvalidRequest(e.to_string()))?;
            req.validate()?;
            Ok(vec![req])
        }
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                return Err(JsonRpcError::InvalidRequest(
                    "batch request cannot be empty".to_string(),
                ));
            }
            let mut batch = Vec::with_capacity(items.len());
            for item in items {
                let req = serde_json::from_value::<JsonRpcRequest>(item)
                    .map_err(|e| JsonRpcError::InvalidRequest(e.to_string()))?;
                req.validate()?;
                batch.push(req);
            }
            Ok(batch)
        }
        _ => Err(JsonRpcError::InvalidRequest(
            "request must be an object or batch array".to_string(),
        )),
    }
}

pub fn parse_single_request(data: &[u8]) -> Result<JsonRpcRequest, JsonRpcError> {
    let requests = parse_request(data)?;
    if requests.len() != 1 {
        return Err(JsonRpcError::InvalidRequest(
            "expected single request".to_string(),
        ));
    }
    Ok(requests.into_iter().next().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_request() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"aria2.addUri","params":["http://example.com/file.iso"]}"#;
        let requests = parse_request(raw.as_bytes()).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "aria2.addUri");
        assert!(!requests[0].is_notification());
    }

    #[test]
    fn test_notification_no_id() {
        let raw =
            r#"{"jsonrpc":"2.0","method":"aria2.onDownloadStart","params":[{"gid":"abc123"}]}"#;
        let requests = parse_request(raw.as_bytes()).unwrap();
        assert!(requests[0].is_notification());
    }

    #[test]
    fn test_batch_requests() {
        let raw = r#"[{"jsonrpc":"2.0","id":1,"method":"aria2.addUri","params":["url1"]},{"jsonrpc":"2.0","id":2,"method":"aria2.addUri","params":["url2"]}]"#;
        let requests = parse_request(raw.as_bytes()).unwrap();
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn test_batch_empty_rejects() {
        let raw = r#"[]"#;
        assert!(parse_request(raw.as_bytes()).is_err());
    }

    #[test]
    fn test_aria2_wire_parser_ignores_version_and_defaults_params() {
        let raw = br#"{"jsonrpc":"1.0","id":1,"method":"aria2.getVersion"}"#;
        let document = parse_aria2_wire_document(raw).unwrap();
        assert!(!document.is_batch);

        let JsonRpcWireEntry::Request(request) = &document.entries[0] else {
            panic!("expected a request entry");
        };
        assert_eq!(request.version.as_deref(), Some("1.0"));
        assert_eq!(request.params, serde_json::json!([]));
    }

    #[test]
    fn test_aria2_wire_parser_materializes_object_errors() {
        let raw = br#"[{"id":1,"method":"aria2.getVersion","params":{}},{"method":"aria2.getVersion"},42,"ignored",[1,2],true,null]"#;
        let document = parse_aria2_wire_document(raw).unwrap();
        assert!(document.is_batch);
        assert_eq!(
            document.entries.len(),
            2,
            "non-object batch items are ignored"
        );

        let JsonRpcWireEntry::Error(error) = &document.entries[0] else {
            panic!("object params must be rejected before dispatch");
        };
        assert_eq!(error.error.as_ref().map(|error| error.code), Some(-32602));
        assert_eq!(error.id, serde_json::json!(1));

        let JsonRpcWireEntry::Error(error) = &document.entries[1] else {
            panic!("missing id must be rejected before dispatch");
        };
        assert_eq!(error.error.as_ref().map(|error| error.code), Some(-32600));
        assert!(error.id.is_null());
    }

    #[test]
    fn test_aria2_wire_parser_preserves_empty_batch() {
        let document = parse_aria2_wire_document(b"[]").unwrap();
        assert!(document.is_batch);
        assert!(document.entries.is_empty());
    }

    #[test]
    fn test_invalid_json() {
        let raw = b"{broken";
        let err = parse_request(raw).unwrap_err();
        assert_eq!(err.code(), -32700);
    }

    #[test]
    fn test_invalid_method() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":""}"#;
        let err = parse_request(raw.as_bytes()).unwrap_err();
        assert_eq!(err.code(), -32600);
    }

    #[test]
    fn test_valid_json_scalar_is_invalid_request() {
        let err = parse_request(b"42").unwrap_err();
        assert_eq!(err.code(), -32600);
    }

    #[test]
    fn test_get_param_positional() {
        let req = JsonRpcRequest::new("test", serde_json::json!(["hello", 42]));
        let s: String = req.get_param(0).unwrap();
        let n: i64 = req.get_param(1).unwrap();
        assert_eq!(s, "hello");
        assert_eq!(n, 42);
    }

    #[test]
    fn test_get_param_named() {
        let req = JsonRpcRequest::new(
            "test",
            serde_json::json!({"uri": "http://example.com", "dir": "/tmp"}),
        );
        let uri: String = req.get_param_by_name("uri").unwrap();
        let dir: String = req.get_param_by_name("dir").unwrap();
        assert_eq!(uri, "http://example.com");
        assert_eq!(dir, "/tmp");
    }

    #[test]
    fn test_get_param_missing() {
        let req = JsonRpcRequest::new("test", serde_json::json!(["only_one"]));
        assert!(req.get_param::<String>(1).is_err());
    }

    #[test]
    fn test_response_success() {
        let resp = JsonRpcResponse::success(1, "2089de05e05901bc1d7d8e048d8d716");
        assert!(resp.is_success());
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap(), "2089de05e05901bc1d7d8e048d8d716");
    }

    #[test]
    fn test_response_error() {
        let resp = JsonRpcResponse::error(1, -32601, "Method not found");
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[test]
    fn test_response_serialize() {
        let resp = JsonRpcResponse::success(42, serde_json::json!({"gid": "abc"}));
        let json = resp.to_string().unwrap();
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"result\""));
    }

    #[test]
    fn test_error_codes() {
        assert_eq!(JsonRpcError::ParseError("x".into()).code(), -32700);
        assert_eq!(JsonRpcError::InvalidRequest("x".into()).code(), -32600);
        assert_eq!(JsonRpcError::MethodNotFound("x".into()).code(), -32601);
        assert_eq!(JsonRpcError::RpcExecution("x".into()).code(), 1);
        assert_eq!(JsonRpcError::InvalidParams("x".into()).code(), -32602);
        assert_eq!(JsonRpcError::InternalError("x".into()).code(), -32603);
        assert_eq!(JsonRpcError::ServerError(-100, "x".into()).code(), -100);
        assert_eq!(JsonRpcError::Unauthorized("x".into()).code(), 1);
    }

    #[test]
    fn test_error_into_response() {
        let err = JsonRpcError::InvalidParams("bad param".to_string());
        let resp = err.into_response(Some(serde_json::json!(1)));
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().message, "bad param");
    }

    #[test]
    fn test_builder_pattern() {
        let req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!(["abc"])).with_id(5);
        assert_eq!(req.method, "aria2.tellStatus");
        assert_eq!(req.id, Some(serde_json::json!(5)));
    }

    #[test]
    fn test_get_param_or_default() {
        let req = JsonRpcRequest::new("test", serde_json::json!([]));
        let val: String = req.get_param_or_default(0);
        assert!(val.is_empty());
    }
}
