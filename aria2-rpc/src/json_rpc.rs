use serde::{Deserialize, Serialize};
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
    /// Authentication failure — token missing or invalid (code: -32001)
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
            Self::Unauthorized(_) => -32001,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::ParseError(s)
            | Self::InvalidRequest(s)
            | Self::MethodNotFound(s)
            | Self::InvalidParams(s)
            | Self::InternalError(s)
            | Self::Unauthorized(s) => s.clone(),
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

    /// Extract and strip the aria2 secret token from request params.
    ///
    /// Implements the original aria2 RPC authentication protocol:
    /// - **Positional params**: if `params[0]` is a string starting with
    ///   `"token:"`, the prefix is stripped and the rest is returned as the
    ///   secret. The token element is removed from the returned params.
    /// - **Named params**: if `params.secret` exists as a string, it is
    ///   returned and removed from the params object.
    /// - Otherwise, returns `(None, params.clone())`.
    ///
    /// The returned params should replace `self.params` before dispatching
    /// to the actual handler so that handlers never see the token.
    ///
    /// Reference: aria2 manual "Authentication" section — both positional
    /// `token:<secret>` and named `secret` are supported.
    pub fn extract_token(params: &serde_json::Value) -> (Option<String>, serde_json::Value) {
        match params {
            serde_json::Value::Array(arr) => {
                if let Some(first) = arr.first()
                    && let Some(s) = first.as_str()
                    && let Some(secret) = s.strip_prefix("token:")
                {
                    // Strip the first element (token) from the array
                    let new_arr: Vec<serde_json::Value> = arr.iter().skip(1).cloned().collect();
                    return (Some(secret.to_string()), serde_json::Value::Array(new_arr));
                }
                (None, params.clone())
            }
            serde_json::Value::Object(map) => {
                if let Some(secret_val) = map.get("secret")
                    && let Some(secret) = secret_val.as_str()
                {
                    let mut new_map = map.clone();
                    new_map.remove("secret");
                    return (Some(secret.to_string()), serde_json::Value::Object(new_map));
                }
                (None, params.clone())
            }
            _ => (None, params.clone()),
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

pub fn parse_request(data: &[u8]) -> Result<Vec<JsonRpcRequest>, JsonRpcError> {
    let parsed: serde_json::Value =
        serde_json::from_slice(data).map_err(|e| JsonRpcError::ParseError(e.to_string()))?;

    if let Ok(req) = serde_json::from_value::<JsonRpcRequest>(parsed.clone()) {
        req.validate()?;
        return Ok(vec![req]);
    }

    if let Ok(batch) = serde_json::from_value::<Vec<JsonRpcRequest>>(parsed) {
        if batch.is_empty() {
            return Err(JsonRpcError::InvalidRequest(
                "batch request cannot be empty".to_string(),
            ));
        }
        for req in &batch {
            req.validate()?;
        }
        return Ok(batch);
    }

    Err(JsonRpcError::ParseError(
        "invalid request format".to_string(),
    ))
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
        assert_eq!(JsonRpcError::InvalidParams("x".into()).code(), -32602);
        assert_eq!(JsonRpcError::InternalError("x".into()).code(), -32603);
        assert_eq!(JsonRpcError::ServerError(-100, "x".into()).code(), -100);
        assert_eq!(JsonRpcError::Unauthorized("x".into()).code(), -32001);
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

    // =========================================================================
    // Token extraction tests (aria2 RPC authentication protocol)
    // =========================================================================

    #[test]
    fn test_extract_token_positional_strips_token_prefix() {
        // Standard positional protocol: params[0] = "token:<secret>"
        let params = serde_json::json!(["token:my-secret", "http://example.com/file.iso"]);
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert_eq!(token.as_deref(), Some("my-secret"));
        // After stripping, params should be the URL array without the token
        assert_eq!(
            new_params,
            serde_json::json!(["http://example.com/file.iso"])
        );
    }

    #[test]
    fn test_extract_token_positional_with_complex_params() {
        // Token + addUri params (uris array + options dict)
        let params = serde_json::json!([
            "token:secret123",
            ["http://example.com/file1.iso", "http://example.com/file2.iso"],
            {"dir": "/tmp", "out": "file.iso"}
        ]);
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert_eq!(token.as_deref(), Some("secret123"));
        assert_eq!(
            new_params,
            serde_json::json!([
                ["http://example.com/file1.iso", "http://example.com/file2.iso"],
                {"dir": "/tmp", "out": "file.iso"}
            ])
        );
    }

    #[test]
    fn test_extract_token_named_secret_field() {
        // Named params protocol: params.secret = "<secret>" (no "token:" prefix)
        let params = serde_json::json!({"secret": "my-secret", "gid": "abc123"});
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert_eq!(token.as_deref(), Some("my-secret"));
        // "secret" field should be removed but "gid" preserved
        assert_eq!(new_params, serde_json::json!({"gid": "abc123"}));
    }

    #[test]
    fn test_extract_token_no_token_in_array_returns_none() {
        // Array without token prefix → no extraction, params unchanged
        let params = serde_json::json!(["http://example.com/file.iso"]);
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert!(token.is_none());
        assert_eq!(new_params, params);
    }

    #[test]
    fn test_extract_token_non_string_first_element_returns_none() {
        // First element is not a string → no extraction
        let params = serde_json::json!([42, "http://example.com/file.iso"]);
        let (token, _) = JsonRpcRequest::extract_token(&params);
        assert!(token.is_none());
    }

    #[test]
    fn test_extract_token_string_without_token_prefix_returns_none() {
        // First element is a string but doesn't start with "token:" → no extraction
        let params = serde_json::json!(["http://example.com/file.iso", "options"]);
        let (token, _) = JsonRpcRequest::extract_token(&params);
        assert!(token.is_none());
    }

    #[test]
    fn test_extract_token_empty_token_secret_returns_empty_string() {
        // Edge case: "token:" with empty secret after prefix
        let params = serde_json::json!(["token:", "http://example.com/file.iso"]);
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert_eq!(token.as_deref(), Some(""));
        assert_eq!(
            new_params,
            serde_json::json!(["http://example.com/file.iso"])
        );
    }

    #[test]
    fn test_extract_token_empty_array_returns_none() {
        let params = serde_json::json!([]);
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert!(token.is_none());
        assert_eq!(new_params, params);
    }

    #[test]
    fn test_extract_token_object_without_secret_returns_none() {
        let params = serde_json::json!({"gid": "abc", "status": "active"});
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert!(token.is_none());
        assert_eq!(new_params, params);
    }

    #[test]
    fn test_extract_token_object_with_non_string_secret_returns_none() {
        // "secret" field exists but is not a string → no extraction
        let params = serde_json::json!({"secret": 123, "gid": "abc"});
        let (token, _) = JsonRpcRequest::extract_token(&params);
        assert!(token.is_none());
    }

    #[test]
    fn test_extract_token_null_params_returns_none() {
        let params = serde_json::Value::Null;
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert!(token.is_none());
        assert_eq!(new_params, params);
    }

    #[test]
    fn test_extract_token_positional_priority_over_named() {
        // When params is an array, named "secret" field is not consulted
        // (params can only be Array OR Object, not both, so no ambiguity)
        let params = serde_json::json!(["token:positional-secret"]);
        let (token, new_params) = JsonRpcRequest::extract_token(&params);
        assert_eq!(token.as_deref(), Some("positional-secret"));
        assert_eq!(new_params, serde_json::json!([]));
    }
}
