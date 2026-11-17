use base64::Engine;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum XmlRpcError {
    ParseError(String),
    InvalidRequest(String),
    MethodNotFound(String),
    InvalidParams(String),
    /// Domain error — fault_code: 1 (matches C++).
    RpcExecution(String),
    ServerFault(i32, String),
}

impl fmt::Display for XmlRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParseError(s) => write!(f, "Parse error: {}", s),
            Self::InvalidRequest(s) => write!(f, "Invalid Request: {}", s),
            Self::MethodNotFound(s) => write!(f, "Method not found: {}", s),
            Self::InvalidParams(s) => write!(f, "Invalid params: {}", s),
            Self::RpcExecution(s) => write!(f, "Error: {}", s),
            Self::ServerFault(c, s) => write!(f, "Server fault ({}): {}", c, s),
        }
    }
}

impl std::error::Error for XmlRpcError {}

impl XmlRpcError {
    pub fn fault_code(&self) -> i32 {
        match self {
            Self::ParseError(_) => -32700,
            Self::InvalidRequest(_) => -32600,
            Self::MethodNotFound(_) => -32601,
            Self::InvalidParams(_) => -32602,
            Self::RpcExecution(_) => 1,
            Self::ServerFault(c, _) => *c,
        }
    }

    pub fn fault_string(&self) -> String {
        match self {
            Self::ParseError(s)
            | Self::InvalidRequest(s)
            | Self::MethodNotFound(s)
            | Self::InvalidParams(s)
            | Self::RpcExecution(s)
            | Self::ServerFault(_, s) => s.clone(),
        }
    }

    pub fn into_response(self) -> XmlRpcResponse {
        XmlRpcResponse::fault(self.fault_code(), &self.fault_string())
    }
}

#[derive(Debug, Clone)]
pub struct XmlRpcValue {
    inner: XmlRpcValueInner,
}

#[derive(Debug, Clone)]
enum XmlRpcValueInner {
    Int(i64),
    Boolean(bool),
    String_(String),
    Double(f64),
    #[allow(dead_code)] // XML-RPC spec type; not yet produced by any response
    DateTime(String),
    #[allow(dead_code)] // XML-RPC spec type; not yet produced by any response
    Base64(Vec<u8>),
    Array(Vec<XmlRpcValue>),
    Struct(Vec<XmlRpcMember>),
    Nil,
}

#[derive(Debug, Clone)]
pub struct XmlRpcMember {
    name: String,
    value: XmlRpcValue,
}

impl XmlRpcValue {
    pub fn int(v: i64) -> Self {
        Self {
            inner: XmlRpcValueInner::Int(v),
        }
    }
    pub fn bool_(v: bool) -> Self {
        Self {
            inner: XmlRpcValueInner::Boolean(v),
        }
    }
    pub fn string(v: impl Into<String>) -> Self {
        Self {
            inner: XmlRpcValueInner::String_(v.into()),
        }
    }
    pub fn double(v: f64) -> Self {
        Self {
            inner: XmlRpcValueInner::Double(v),
        }
    }
    pub fn array(v: Vec<XmlRpcValue>) -> Self {
        Self {
            inner: XmlRpcValueInner::Array(v),
        }
    }
    pub fn struct_(v: Vec<XmlRpcMember>) -> Self {
        Self {
            inner: XmlRpcValueInner::Struct(v),
        }
    }
    pub fn nil() -> Self {
        Self {
            inner: XmlRpcValueInner::Nil,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        if let XmlRpcValueInner::Int(v) = &self.inner {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let XmlRpcValueInner::String_(s) = &self.inner {
            Some(s.as_str())
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let XmlRpcValueInner::Boolean(b) = &self.inner {
            Some(*b)
        } else {
            None
        }
    }
    pub fn as_double(&self) -> Option<f64> {
        if let XmlRpcValueInner::Double(value) = &self.inner {
            Some(*value)
        } else {
            None
        }
    }
    pub fn as_array(&self) -> Option<&Vec<XmlRpcValue>> {
        if let XmlRpcValueInner::Array(a) = &self.inner {
            Some(a)
        } else {
            None
        }
    }
    pub fn as_struct(&self) -> Option<&[XmlRpcMember]> {
        if let XmlRpcValueInner::Struct(members) = &self.inner {
            Some(members)
        } else {
            None
        }
    }
    pub fn is_nil(&self) -> bool {
        matches!(&self.inner, XmlRpcValueInner::Nil)
    }
}

impl XmlRpcMember {
    pub fn new(name: impl Into<String>, value: XmlRpcValue) -> Self {
        Self {
            name: name.into(),
            value,
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn value(&self) -> &XmlRpcValue {
        &self.value
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn value_to_xml(v: &XmlRpcValue, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match &v.inner {
        XmlRpcValueInner::Int(n) => format!("{}<value><int>{}</int></value>", pad, n),
        XmlRpcValueInner::Boolean(b) => format!("{}<value><boolean>{}</boolean></value>", pad, b),
        XmlRpcValueInner::String_(s) => {
            format!("{}<value><string>{}</string></value>", pad, escape_xml(s))
        }
        XmlRpcValueInner::Double(d) => format!("{}<value><double>{}</double></value>", pad, d),
        XmlRpcValueInner::DateTime(dt) => format!(
            "{}<value><dateTime.iso8601>{}</dateTime.iso8601></value>",
            pad, dt
        ),
        XmlRpcValueInner::Base64(data) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(data);
            format!("{}<value><base64>{}</base64></value>", pad, encoded)
        }
        XmlRpcValueInner::Nil => format!("{}<value><nil/></value>", pad),
        XmlRpcValueInner::Array(arr) => {
            let mut parts = vec![format!("{}<value><array><data>", pad)];
            for item in arr {
                parts.push(value_to_xml(item, indent + 2));
            }
            parts.push(format!("{}</data></array></value>", pad));
            parts.join("\n")
        }
        XmlRpcValueInner::Struct(members) => {
            let mut parts = vec![format!("{}<value><struct>", pad)];
            for m in members {
                parts.push(format!(
                    "{}  <member><name>{}</name>",
                    pad,
                    escape_xml(&m.name)
                ));
                parts.push(value_to_xml(&m.value, indent + 4));
                parts.push(format!("{}</member>", pad));
            }
            parts.push(format!("{}</struct></value>", pad));
            parts.join("\n")
        }
    }
}

impl XmlRpcValue {
    pub fn to_xml(&self) -> String {
        value_to_xml(self, 0)
    }

    /// Convert an XML-RPC value to the JSON value shape consumed by the
    /// shared RPC engine. Binary XML-RPC values remain base64 strings because
    /// aria2's JSON methods use base64 for uploaded torrent/Metalink data.
    pub fn to_json_value(&self) -> Result<serde_json::Value, XmlRpcError> {
        match &self.inner {
            XmlRpcValueInner::Int(value) => Ok(serde_json::json!(value)),
            XmlRpcValueInner::Boolean(value) => Ok(serde_json::json!(value)),
            XmlRpcValueInner::String_(value) | XmlRpcValueInner::DateTime(value) => {
                Ok(serde_json::Value::String(value.clone()))
            }
            // C++ aria2 parses XML-RPC <double> using its string state and
            // forwards the textual value to the RPC method. Keep that wire
            // contract even though the value type remains available for
            // Rust-side XML-RPC response construction.
            XmlRpcValueInner::Double(value) => Ok(serde_json::Value::String(value.to_string())),
            XmlRpcValueInner::Base64(data) => Ok(serde_json::Value::String(
                base64::engine::general_purpose::STANDARD.encode(data),
            )),
            XmlRpcValueInner::Array(values) => values
                .iter()
                .map(Self::to_json_value)
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array),
            XmlRpcValueInner::Struct(members) => {
                let mut object = serde_json::Map::with_capacity(members.len());
                for member in members {
                    object.insert(member.name.clone(), member.value.to_json_value()?);
                }
                Ok(serde_json::Value::Object(object))
            }
            XmlRpcValueInner::Nil => Ok(serde_json::Value::Null),
        }
    }

    /// Construct an XML-RPC value from a JSON-RPC response value.
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, XmlRpcError> {
        match value {
            serde_json::Value::Null => Ok(Self::nil()),
            serde_json::Value::Bool(value) => Ok(Self::bool_(value)),
            serde_json::Value::String(value) => Ok(Self::string(value)),
            serde_json::Value::Number(value) => {
                if let Some(integer) = value.as_i64() {
                    Ok(Self::int(integer))
                } else if let Some(double) = value.as_f64() {
                    Ok(Self::double(double))
                } else {
                    Err(XmlRpcError::InvalidParams(
                        "JSON number is outside XML-RPC integer range".into(),
                    ))
                }
            }
            serde_json::Value::Array(values) => values
                .into_iter()
                .map(Self::from_json_value)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::array),
            serde_json::Value::Object(object) => object
                .into_iter()
                .map(|(name, value)| {
                    Self::from_json_value(value).map(|value| XmlRpcMember::new(name, value))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Self::struct_),
        }
    }
}

#[derive(Debug, Clone)]
pub struct XmlRpcRequest {
    pub method_name: String,
    pub params: Vec<XmlRpcValue>,
}

impl XmlRpcRequest {
    pub fn new(method: impl Into<String>, params: Vec<XmlRpcValue>) -> Self {
        Self {
            method_name: method.into(),
            params,
        }
    }

    pub fn to_xml(&self) -> String {
        let mut parts = vec![
            "<?xml version=\"1.0\"?>".to_string(),
            "<methodCall>".to_string(),
        ];
        parts.push(format!("  <methodName>{}</methodName>", self.method_name));
        parts.push("  <params>".to_string());
        for p in &self.params {
            parts.push(format!("    <param>{}</param>", p.to_xml()));
        }
        parts.push("  </params>".to_string());
        parts.push("</methodCall>".to_string());
        parts.join("\n")
    }

    pub fn get_param(&self, index: usize) -> Result<&XmlRpcValue, XmlRpcError> {
        self.params
            .get(index)
            .ok_or_else(|| XmlRpcError::InvalidParams(format!("param[{}] missing", index)))
    }
}

#[derive(Debug, Clone)]
pub enum XmlRpcResponse {
    Success(Vec<XmlRpcValue>),
    Fault(i32, String),
}

impl XmlRpcResponse {
    pub fn success(values: Vec<XmlRpcValue>) -> Self {
        Self::Success(values)
    }
    pub fn single(value: XmlRpcValue) -> Self {
        Self::Success(vec![value])
    }
    pub fn string_val(value: impl Into<String>) -> Self {
        Self::single(XmlRpcValue::string(value))
    }
    pub fn int_val(value: i64) -> Self {
        Self::single(XmlRpcValue::int(value))
    }
    pub fn bool_val(value: bool) -> Self {
        Self::single(XmlRpcValue::bool_(value))
    }
    pub fn array_val(values: Vec<XmlRpcValue>) -> Self {
        Self::single(XmlRpcValue::array(values))
    }
    pub fn fault(code: i32, msg: &str) -> Self {
        Self::Fault(code, msg.to_string())
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::Fault(1, format!("Method '{}' not found", method))
    }
    pub fn invalid_params(msg: &str) -> Self {
        Self::Fault(-32602, msg.to_string())
    }
    pub fn parse_error(msg: &str) -> Self {
        Self::Fault(-32700, msg.to_string())
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }
    pub fn is_fault(&self) -> bool {
        matches!(self, Self::Fault(..))
    }

    pub fn to_xml(&self) -> String {
        match self {
            Self::Success(params) => {
                let mut parts = vec![
                    "<?xml version=\"1.0\"?>".to_string(),
                    "<methodResponse>".to_string(),
                ];
                parts.push("  <params>".to_string());
                for p in params {
                    parts.push(format!("    <param>{}</param>", p.to_xml()));
                }
                parts.push("  </params>".to_string());
                parts.push("</methodResponse>".to_string());
                parts.join("\n")
            }
            Self::Fault(code, msg) => {
                format!(
                    "<?xml version=\"1.0\"?>\n<methodResponse>\n  <fault>\n    <value>\n      <struct>\n        <member><name>faultCode</name><value><int>{}</int></value></member>\n        <member><name>faultString</name><value><string>{}</string></value></member>\n      </struct>\n    </value>\n  </fault>\n</methodResponse>",
                    code,
                    escape_xml(msg)
                )
            }
        }
    }
}

mod parser;
pub use parser::parse_request;

#[cfg(test)]
mod tests;
