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

    /// Consume one positional or named parameter without cloning its JSON
    /// value. Network-facing dispatch owns the request document, so moving a
    /// large URI list, options map, or encoded payload is cheaper than
    /// materializing a second DOM subtree for `serde_json::from_value`.
    pub(crate) fn take_param<T: serde::de::DeserializeOwned>(
        &mut self,
        index: usize,
    ) -> Result<T, JsonRpcError> {
        let value = match &mut self.params {
            serde_json::Value::Array(arr) if index < arr.len() => {
                std::mem::replace(&mut arr[index], serde_json::Value::Null)
            }
            serde_json::Value::Object(map) => {
                let key = format!("p{index}");
                map.remove(&key).ok_or_else(|| {
                    JsonRpcError::InvalidParams(format!("param[{}] missing", index))
                })?
            }
            _ => {
                return Err(JsonRpcError::InvalidParams(format!(
                    "param[{}] not found",
                    index
                )));
            }
        };

        serde_json::from_value(value).map_err(|error| {
            JsonRpcError::InvalidParams(format!("param[{}] type error: {}", index, error))
        })
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

    /// Consume an optional parameter without cloning its JSON value.
    pub(crate) fn take_optional_param<T: serde::de::DeserializeOwned>(
        &mut self,
        index: usize,
    ) -> Result<Option<T>, JsonRpcError> {
        let value = match &mut self.params {
            serde_json::Value::Array(arr) => {
                let Some(value) = arr.get_mut(index) else {
                    return Ok(None);
                };
                std::mem::replace(value, serde_json::Value::Null)
            }
            serde_json::Value::Object(map) => {
                let key = format!("p{index}");
                let Some(value) = map.remove(&key) else {
                    return Ok(None);
                };
                value
            }
            _ => return Ok(None),
        };

        serde_json::from_value(value).map(Some).map_err(|error| {
            JsonRpcError::InvalidParams(format!("param[{}] type error: {}", index, error))
        })
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

mod wire;
pub use wire::{JsonRpcWireDocument, JsonRpcWireEntry, parse_aria2_wire_document};

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
mod tests;
