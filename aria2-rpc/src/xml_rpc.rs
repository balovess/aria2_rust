use serde::{Deserialize, Serialize};
use std::fmt;
use base64::Engine;

#[derive(Debug, Clone, PartialEq)]
pub enum XmlRpcError {
    ParseError(String),
    InvalidRequest(String),
    MethodNotFound(String),
    InvalidParams(String),
    ServerFault(i32, String),
}

impl fmt::Display for XmlRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParseError(s) => write!(f, "Parse error: {}", s),
            Self::InvalidRequest(s) => write!(f, "Invalid Request: {}", s),
            Self::MethodNotFound(s) => write!(f, "Method not found: {}", s),
            Self::InvalidParams(s) => write!(f, "Invalid params: {}", s),
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
            Self::ServerFault(c, _) => *c,
        }
    }

    pub fn fault_string(&self) -> String {
        match self {
            Self::ParseError(s) | Self::InvalidRequest(s) |
            Self::MethodNotFound(s) | Self::InvalidParams(s) |
            Self::ServerFault(_, s) => s.clone(),
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
    DateTime(String),
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
    pub fn int(v: i64) -> Self { Self { inner: XmlRpcValueInner::Int(v) } }
    pub fn bool_(v: bool) -> Self { Self { inner: XmlRpcValueInner::Boolean(v) } }
    pub fn string(v: impl Into<String>) -> Self { Self { inner: XmlRpcValueInner::String_(v.into()) } }
    pub fn double(v: f64) -> Self { Self { inner: XmlRpcValueInner::Double(v) } }
    pub fn array(v: Vec<XmlRpcValue>) -> Self { Self { inner: XmlRpcValueInner::Array(v) } }
    pub fn struct_(v: Vec<XmlRpcMember>) -> Self { Self { inner: XmlRpcValueInner::Struct(v) } }
    pub fn nil() -> Self { Self { inner: XmlRpcValueInner::Nil } }

    pub fn as_i64(&self) -> Option<i64> {
        if let XmlRpcValueInner::Int(v) = &self.inner { Some(*v) } else { None }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let XmlRpcValueInner::String_(s) = &self.inner { Some(s.as_str()) } else { None }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let XmlRpcValueInner::Boolean(b) = &self.inner { Some(*b) } else { None }
    }
    pub fn as_array(&self) -> Option<&Vec<XmlRpcValue>> {
        if let XmlRpcValueInner::Array(a) = &self.inner { Some(a) } else { None }
    }
    pub fn is_nil(&self) -> bool { matches!(&self.inner, XmlRpcValueInner::Nil) }
}

impl XmlRpcMember {
    pub fn new(name: impl Into<String>, value: XmlRpcValue) -> Self {
        Self { name: name.into(), value }
    }
    pub fn name(&self) -> &str { &self.name }
    pub fn value(&self) -> &XmlRpcValue { &self.value }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('"', "&quot;").replace('\'', "&apos;")
}

fn value_to_xml(v: &XmlRpcValue, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match &v.inner {
        XmlRpcValueInner::Int(n) => format!("{}<value><int>{}</int></value>", pad, n),
        XmlRpcValueInner::Boolean(b) => format!("{}<value><boolean>{}</boolean></value>", pad, b),
        XmlRpcValueInner::String_(s) => format!("{}<value><string>{}</string></value>", pad, escape_xml(s)),
        XmlRpcValueInner::Double(d) => format!("{}<value><double>{}</double></value>", pad, d),
        XmlRpcValueInner::DateTime(dt) => format!("{}<value><dateTime.iso8601>{}</dateTime.iso8601></value>", pad, dt),
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
                parts.push(format!("{}  <member><name>{}</name>", pad, escape_xml(&m.name)));
                parts.push(value_to_xml(&m.value, indent + 4));
                parts.push(format!("{}</member>", pad));
            }
            parts.push(format!("{}</struct></value>", pad));
            parts.join("\n")
        }
    }
}

impl XmlRpcValue {
    pub fn to_xml(&self) -> String { value_to_xml(self, 0) }
}

#[derive(Debug, Clone)]
pub struct XmlRpcRequest {
    pub method_name: String,
    pub params: Vec<XmlRpcValue>,
}

impl XmlRpcRequest {
    pub fn new(method: impl Into<String>, params: Vec<XmlRpcValue>) -> Self {
        Self { method_name: method.into(), params }
    }

    pub fn to_xml(&self) -> String {
        let mut parts = vec!["<?xml version=\"1.0\"?>".to_string(), "<methodCall>".to_string()];
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
        self.params.get(index).ok_or_else(|| XmlRpcError::InvalidParams(format!("param[{}] missing", index)))
    }
}

#[derive(Debug, Clone)]
pub enum XmlRpcResponse {
    Success(Vec<XmlRpcValue>),
    Fault(i32, String),
}

impl XmlRpcResponse {
    pub fn success(values: Vec<XmlRpcValue>) -> Self { Self::Success(values) }
    pub fn single(value: XmlRpcValue) -> Self { Self::Success(vec![value]) }
    pub fn string_val(value: impl Into<String>) -> Self { Self::single(XmlRpcValue::string(value)) }
    pub fn int_val(value: i64) -> Self { Self::single(XmlRpcValue::int(value)) }
    pub fn bool_val(value: bool) -> Self { Self::single(XmlRpcValue::bool_(value)) }
    pub fn array_val(values: Vec<XmlRpcValue>) -> Self { Self::success(values) }
    pub fn fault(code: i32, msg: &str) -> Self { Self::Fault(code, msg.to_string()) }
    pub fn method_not_found(method: &str) -> Self { Self::Fault(-32601, format!("Method '{}' not found", method)) }
    pub fn invalid_params(msg: &str) -> Self { Self::Fault(-32602, msg.to_string()) }
    pub fn parse_error(msg: &str) -> Self { Self::Fault(-32700, msg.to_string()) }

    pub fn is_success(&self) -> bool { matches!(self, Self::Success(_)) }
    pub fn is_fault(&self) -> bool { matches!(self, Self::Fault(..)) }

    pub fn to_xml(&self) -> String {
        match self {
            Self::Success(params) => {
                let mut parts = vec!["<?xml version=\"1.0\"?>".to_string(), "<methodResponse>".to_string()];
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
                    code, escape_xml(msg)
                )
            }
        }
    }
}

fn parse_value(e: &quick_xml::events::BytesStart) -> Result<XmlRpcValue, XmlRpcError> {
    let tag_bytes = e.local_name();
    let tag = std::str::from_utf8(tag_bytes.as_ref()).unwrap_or("");
    match tag {
        "int" | "i4" | "i8" => {
            let text = e.attributes().flatten().next().map(|a| std::str::from_utf8(&a.value).unwrap_or("0").to_string()).unwrap_or_else(|| "0".to_string());
            Ok(XmlRpcValue::int(text.trim().parse::<i64>().unwrap_or(0)))
        }
        "boolean" => {
            let text = e.attributes().flatten().next().map(|a| std::str::from_utf8(&a.value).unwrap_or("0").to_string()).unwrap_or_else(|| "0".to_string());
            let val = text.trim() == "1" || text.trim().to_lowercase() == "true";
            Ok(XmlRpcValue::bool_(val))
        }
        "string" => {
            let text = e.attributes().flatten().next().map(|a| std::str::from_utf8(&a.value).unwrap_or("").to_string()).unwrap_or_else(|| String::new());
            Ok(XmlRpcValue::string(text))
        }
        "double" => {
            let text = e.attributes().flatten().next().map(|a| std::str::from_utf8(&a.value).unwrap_or("0.0").to_string()).unwrap_or_else(|| "0.0".to_string());
            Ok(XmlRpcValue::double(text.trim().parse::<f64>().unwrap_or(0.0)))
        }
        "array" => Ok(XmlRpcValue::array(vec![])),
        "struct" => Ok(XmlRpcValue::array(vec![])),
        "nil" => Ok(XmlRpcValue::nil()),
        _ => Err(XmlRpcError::ParseError(format!("unknown XML-RPC type: {}", tag))),
    }
}

pub fn parse_request(data: &[u8]) -> Result<XmlRpcRequest, XmlRpcError> {
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_reader(data);
    let mut method_name = String::new();
    let mut params = Vec::new();
    let mut in_params = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let tag_bytes = e.local_name();
                let tag = std::str::from_utf8(tag_bytes.as_ref()).unwrap_or("");
                match tag {
                    "methodName" => {
                        let text = reader.read_text(e.name()).unwrap_or_default();
                        method_name = text.to_string();
                    }
                    "params" => in_params = true,
                    "value" => { /* wrapper tag, inner type will be handled next */ }
                    "int" | "string" | "boolean" | "double" | "array" | "struct" | "nil" => {
                        if in_params {
                            params.push(parse_value(e)?);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(XmlRpcError::ParseError(e.to_string())),
            _ => {}
        }
    }
    if method_name.is_empty() {
        return Err(XmlRpcError::InvalidRequest("methodName is required".to_string()));
    }
    Ok(XmlRpcRequest::new(method_name, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_int_value_to_xml() {
        let v = XmlRpcValue::int(42);
        let xml = v.to_xml();
        assert!(xml.contains("<int>42</int>"));
    }

    #[test]
    fn test_string_value_to_xml() {
        let v = XmlRpcValue::string("hello world");
        let xml = v.to_xml();
        assert!(xml.contains("<string>hello world</string>"));
    }

    #[test]
    fn test_array_value_to_xml() {
        let v = XmlRpcValue::array(vec![XmlRpcValue::int(1), XmlRpcValue::string("test")]);
        let xml = v.to_xml();
        assert!(xml.contains("<array>"));
        assert!(xml.contains("<data>"));
    }

    #[test]
    fn test_request_to_xml() {
        let req = XmlRpcRequest::new("aria2.addUri", vec![
            XmlRpcValue::string("http://example.com/file.iso"),
            XmlRpcValue::array(vec![XmlRpcValue::struct_(vec![XmlRpcMember::new("dir", XmlRpcValue::string("/downloads"))])]),
        ]);
        let xml = req.to_xml();
        assert!(xml.contains("<methodName>aria2.addUri</methodName>"));
        assert!(xml.contains("<string>http://example.com/file.iso</string>"));
    }

    #[test]
    fn test_response_success() {
        let resp = XmlRpcResponse::string_val("2089de05e05901bc1d7d8e048d8d716");
        let xml = resp.to_xml();
        assert!(resp.is_success());
        assert!(xml.contains("<methodResponse>"));
        assert!(xml.contains("<params>"));
    }

    #[test]
    fn test_response_fault() {
        let resp = XmlRpcResponse::fault(-32601, "Method not found");
        let xml = resp.to_xml();
        assert!(resp.is_fault());
        assert!(xml.contains("<fault>"));
        assert!(xml.contains("<name>faultCode</name>"));
        assert!(xml.contains("<int>-32601</int>"));
    }

    #[test]
    fn test_error_codes() {
        assert_eq!(XmlRpcError::ParseError("x".into()).fault_code(), -32700);
        assert_eq!(XmlRpcError::MethodNotFound("x".into()).fault_code(), -32601);
        assert_eq!(XmlRpcError::InvalidParams("x".into()).fault_code(), -32602);
        assert_eq!(XmlRpcError::ServerFault(400, "x".into()).fault_code(), 400);
    }

    #[test]
    fn test_member_accessors() {
        let m = XmlRpcMember::new("dir", XmlRpcValue::string("/tmp"));
        assert_eq!(m.name(), "dir");
        assert_eq!(m.value().as_str().unwrap(), "/tmp");
    }

    #[test]
    fn test_value_accessors() {
        let v = XmlRpcValue::int(99);
        assert_eq!(v.as_i64().unwrap(), 99);
        assert!(v.as_str().is_none());

        let s = XmlRpcValue::string("test");
        assert_eq!(s.as_str().unwrap(), "test");

        let b = XmlRpcValue::bool_(true);
        assert!(b.as_bool().unwrap());

        let n = XmlRpcValue::nil();
        assert!(n.is_nil());
    }

    #[test]
    fn test_parse_simple_request() {
        let xml = r#"<?xml version="1.0"?>
<methodCall>
  <methodName>aria2.addUri</methodName>
  <params>
    <param><value><string>http://example.com/file.iso</string></value></param>
  </params>
</methodCall>"#;
        let req = parse_request(xml.as_bytes()).unwrap();
        assert_eq!(req.method_name, "aria2.addUri");
        assert_eq!(req.params.len(), 1);
    }

    #[test]
    fn test_escape_xml_special_chars() {
        let escaped = escape_xml("a<b&c>d\"e'f");
        assert!(escaped.contains("&lt;"));
        assert!(escaped.contains("&gt;"));
        assert!(escaped.contains("&amp;"));
        assert!(escaped.contains("&quot;"));
    }
}
