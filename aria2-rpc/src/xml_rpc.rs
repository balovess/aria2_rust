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

fn xml_tag(e: &quick_xml::events::BytesStart<'_>) -> Result<String, XmlRpcError> {
    std::str::from_utf8(e.local_name().as_ref())
        .map(str::to_owned)
        .map_err(|e| XmlRpcError::ParseError(e.to_string()))
}

fn read_scalar(
    reader: &mut quick_xml::Reader<&[u8]>,
    e: &quick_xml::events::BytesStart<'_>,
    tag: &str,
) -> Result<Option<XmlRpcValue>, XmlRpcError> {
    let text = reader
        .read_text(e.name())
        .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
        .into_owned();
    let numeric_text = text.trim();
    match tag {
        // aria2_original stores XML-RPC int/i4 as its 32-bit Integer value.
        // A syntactically valid but out-of-range integer leaves the parser
        // frame empty; the enclosing param/member then omits that value.
        "int" | "i4" => Ok(numeric_text
            .parse::<i32>()
            .ok()
            .map(i64::from)
            .map(XmlRpcValue::int)),
        // Keep the standard i8 extension accepted by the Rust adapter. It is
        // an additive wire type and does not change the original int/i4 path.
        "i8" => Ok(numeric_text.parse::<i64>().ok().map(XmlRpcValue::int)),
        "boolean" => match numeric_text {
            "1" | "true" => Ok(Some(XmlRpcValue::bool_(true))),
            "0" | "false" => Ok(Some(XmlRpcValue::bool_(false))),
            _ => Ok(None),
        },
        // aria2_original's XML-RPC parser keeps both explicit string values
        // and double values as strings. The latter is observable for option
        // maps such as {"seed-ratio": <double>0.99</double>}.
        "string" | "double" => Ok(Some(XmlRpcValue::string(text))),
        "dateTime.iso8601" => Ok(Some(XmlRpcValue {
            inner: XmlRpcValueInner::DateTime(text),
        })),
        "base64" => Ok(Some(XmlRpcValue {
            inner: XmlRpcValueInner::Base64(crate::rpc_helpers::decode_aria2_base64(&text)),
        })),
        _ => Ok(None),
    }
}

fn parse_xml_value(
    reader: &mut quick_xml::Reader<&[u8]>,
    value_start: &quick_xml::events::BytesStart<'_>,
) -> Result<Option<XmlRpcValue>, XmlRpcError> {
    use quick_xml::events::Event;
    let mut implicit_text = String::new();

    loop {
        match reader
            .read_event()
            .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
        {
            Event::Start(e) => {
                let tag = xml_tag(&e)?;
                let value = match tag.as_str() {
                    "array" => Some(parse_xml_array(reader, &e)?),
                    "struct" => Some(parse_xml_struct(reader, &e)?),
                    "nil" => {
                        reader
                            .read_to_end(e.name())
                            .map_err(|e| XmlRpcError::ParseError(e.to_string()))?;
                        Some(XmlRpcValue::nil())
                    }
                    scalar => read_scalar(reader, &e, scalar)?,
                };

                if value.is_some() {
                    reader
                        .read_to_end(value_start.name())
                        .map_err(|e| XmlRpcError::ParseError(e.to_string()))?;
                    return Ok(value);
                }

                // Invalid known scalar nodes have already been consumed by
                // read_scalar. The C++ state machine then continues parsing
                // the enclosing <value> frame instead of failing the whole
                // XML request.
            }
            Event::Empty(e) => {
                let tag = xml_tag(&e)?;
                let value = match tag.as_str() {
                    "array" => Some(XmlRpcValue::array(Vec::new())),
                    "struct" => Some(XmlRpcValue::struct_(Vec::new())),
                    "nil" => Some(XmlRpcValue::nil()),
                    "string" | "double" | "dateTime.iso8601" => Some(XmlRpcValue::string("")),
                    "base64" => Some(XmlRpcValue {
                        inner: XmlRpcValueInner::Base64(Vec::new()),
                    }),
                    // Empty numeric/boolean values are invalid in the
                    // original parser and therefore leave no frame value.
                    "int" | "i4" | "i8" | "boolean" => None,
                    _ => None,
                };
                if value.is_some() {
                    reader
                        .read_to_end(value_start.name())
                        .map_err(|e| XmlRpcError::ParseError(e.to_string()))?;
                    return Ok(value);
                }
            }
            Event::Text(text) => {
                let text = text
                    .unescape()
                    .map_err(|e| XmlRpcError::ParseError(e.to_string()))?;
                implicit_text.push_str(&text);
            }
            Event::CData(text) => {
                implicit_text.push_str(&String::from_utf8_lossy(text.as_ref()));
            }
            Event::End(e) if e.name() == value_start.name() => {
                return Ok((!implicit_text.is_empty()).then(|| XmlRpcValue::string(implicit_text)));
            }
            Event::Eof => return Err(XmlRpcError::ParseError("unexpected end of value".into())),
            _ => {}
        }
    }
}

fn parse_xml_array(
    reader: &mut quick_xml::Reader<&[u8]>,
    array_start: &quick_xml::events::BytesStart<'_>,
) -> Result<XmlRpcValue, XmlRpcError> {
    use quick_xml::events::Event;
    let mut values = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
        {
            Event::Start(e) if xml_tag(&e)? == "value" => {
                if let Some(value) = parse_xml_value(reader, &e)? {
                    values.push(value);
                }
            }
            Event::End(e) if e.name() == array_start.name() => {
                return Ok(XmlRpcValue::array(values));
            }
            Event::Eof => return Err(XmlRpcError::ParseError("unexpected end of array".into())),
            _ => {}
        }
    }
}

fn parse_xml_struct(
    reader: &mut quick_xml::Reader<&[u8]>,
    struct_start: &quick_xml::events::BytesStart<'_>,
) -> Result<XmlRpcValue, XmlRpcError> {
    use quick_xml::events::Event;
    let mut members = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
        {
            Event::Start(member) if xml_tag(&member)? == "member" => {
                let mut name = None;
                let mut value = None;
                loop {
                    match reader
                        .read_event()
                        .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
                    {
                        Event::Start(e) if xml_tag(&e)? == "name" => {
                            name = Some(
                                reader
                                    .read_text(e.name())
                                    .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
                                    .into_owned(),
                            );
                        }
                        Event::Start(e) if xml_tag(&e)? == "value" => {
                            value = parse_xml_value(reader, &e)?
                        }
                        Event::Start(e) => {
                            reader
                                .read_to_end(e.name())
                                .map_err(|e| XmlRpcError::ParseError(e.to_string()))?;
                        }
                        Event::End(e) if e.name() == member.name() => break,
                        Event::Eof => {
                            return Err(XmlRpcError::ParseError("unexpected end of member".into()));
                        }
                        _ => {}
                    }
                }
                if let (Some(name), Some(value)) = (name, value)
                    && !name.is_empty()
                {
                    members.push(XmlRpcMember::new(name, value));
                }
            }
            Event::End(e) if e.name() == struct_start.name() => {
                return Ok(XmlRpcValue::struct_(members));
            }
            Event::Eof => return Err(XmlRpcError::ParseError("unexpected end of struct".into())),
            _ => {}
        }
    }
}

pub fn parse_request(data: &[u8]) -> Result<XmlRpcRequest, XmlRpcError> {
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_reader(data);
    let mut method_name = None;
    let mut params = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
        {
            Event::Start(e) if xml_tag(&e)? == "methodName" => {
                method_name = Some(
                    reader
                        .read_text(e.name())
                        .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
                        .into_owned(),
                );
            }
            Event::Start(e) if xml_tag(&e)? == "param" => loop {
                match reader
                    .read_event()
                    .map_err(|e| XmlRpcError::ParseError(e.to_string()))?
                {
                    Event::Start(value) if xml_tag(&value)? == "value" => {
                        if let Some(value) = parse_xml_value(&mut reader, &value)? {
                            params.push(value);
                        }
                        reader
                            .read_to_end(e.name())
                            .map_err(|e| XmlRpcError::ParseError(e.to_string()))?;
                        break;
                    }
                    Event::End(end) if end.name() == e.name() => break,
                    Event::Eof => {
                        return Err(XmlRpcError::ParseError("unexpected end of param".into()));
                    }
                    _ => {}
                }
            },
            Event::Eof => break,
            _ => {}
        }
    }
    let method_name =
        method_name.ok_or_else(|| XmlRpcError::InvalidRequest("methodName is required".into()))?;
    if method_name.is_empty() {
        return Err(XmlRpcError::InvalidRequest("methodName is required".into()));
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
        let req = XmlRpcRequest::new(
            "aria2.addUri",
            vec![
                XmlRpcValue::string("http://example.com/file.iso"),
                XmlRpcValue::array(vec![XmlRpcValue::struct_(vec![XmlRpcMember::new(
                    "dir",
                    XmlRpcValue::string("/downloads"),
                )])]),
            ],
        );
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
        assert_eq!(XmlRpcError::RpcExecution("x".into()).fault_code(), 1);
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
    fn test_parse_nested_array_and_struct_without_parameter_leakage() {
        let xml = r#"<methodCall><methodName>aria2.addUri</methodName><params><param><value><array><data>
            <value><string>one</string></value>
            <value><struct><member><name>dir</name><value><string>/downloads</string></value></member>
                <member><name>timeout</name><value><int>120</int></value></member>
            </struct></value>
        </data></array></value></param></params></methodCall>"#;
        let req = parse_request(xml.as_bytes()).unwrap();
        assert_eq!(req.params.len(), 1);
        let items = req.params[0].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_str(), Some("one"));
        let members = items[1].as_struct().unwrap();
        assert_eq!(members[0].name(), "dir");
        assert_eq!(members[0].value().as_str(), Some("/downloads"));
        assert_eq!(members[1].value().as_i64(), Some(120));
    }

    #[test]
    fn test_parse_struct_with_nested_array_and_implicit_string() {
        let xml = r#"<methodCall><methodName>aria2.changeOption</methodName><params><param><value><struct>
            <member><name>uris</name><value><array><data><value>jp</value><value><string>us</string></value></data></array></value></member>
            <member><name>enabled</name><value><boolean>1</boolean></value></member>
        </struct></value></param></params></methodCall>"#;
        let req = parse_request(xml.as_bytes()).unwrap();
        assert_eq!(req.params.len(), 1);
        let members = req.params[0].as_struct().unwrap();
        assert_eq!(
            members[0].value().as_array().unwrap()[0].as_str(),
            Some("jp")
        );
        assert_eq!(
            members[0].value().as_array().unwrap()[1].as_str(),
            Some("us")
        );
        assert_eq!(members[1].value().as_bool(), Some(true));
    }

    #[test]
    fn test_parse_scalar_text_values() {
        let xml = r#"<methodCall><methodName>test</methodName><params>
            <param><value><int>100</int></value></param>
            <param><value><double>0.5</double></value></param>
            <param><value><boolean>0</boolean></value></param>
        </params></methodCall>"#;
        let req = parse_request(xml.as_bytes()).unwrap();
        assert_eq!(req.params.len(), 3);
        assert_eq!(req.params[0].as_i64(), Some(100));
        assert_eq!(req.params[1].as_str(), Some("0.5"));
        assert_eq!(req.params[2].as_bool(), Some(false));
    }

    #[test]
    fn test_xmlrpc_wire_conversion_preserves_strings_and_coerces_doubles() {
        let xml = r#"<methodCall><methodName>aria2.changeOption</methodName><params>
            <param><value><struct>
                <member><name>dir</name><value><string>  /downloads  </string></value></member>
                <member><name>seed-ratio</name><value><double>0.99</double></value></member>
            </struct></value></param>
        </params></methodCall>"#;
        let req = parse_request(xml.as_bytes()).unwrap();
        let value = req.params[0].to_json_value().unwrap();

        assert_eq!(value["dir"], serde_json::json!("  /downloads  "));
        assert_eq!(value["seed-ratio"], serde_json::json!("0.99"));
    }

    #[test]
    fn test_parse_base64_uses_original_permissive_wire_rules() {
        let xml = r#"<methodCall><methodName>aria2.addTorrent</methodName><params>
            <param><value><base64>aG Vs
bG8g@d29ybGQ=</base64></value></param>
        </params></methodCall>"#;

        let request = parse_request(xml.as_bytes()).expect("XML-RPC request should parse");
        let value = request.params[0]
            .to_json_value()
            .expect("base64 value should convert to JSON");

        assert_eq!(value, serde_json::json!("aGVsbG8gd29ybGQ="));
    }

    #[test]
    fn test_parse_invalid_values_and_members_like_original_state_machine() {
        let xml = r#"<methodCall><methodName>aria2.addUri</methodName><params>
            <param><value><int>not-an-int</int></value></param>
            <param><value><int>2147483648</int></value></param>
            <param><value><unknown>ignored</unknown></value></param>
            <param><value><foo/><string>kept</string></value></param>
            <param><value><struct>
                <member><name>missing-value</name></member>
                <member><value><string>missing-name</string></value></member>
                <member><name></name><value><string>empty-name</string></value></member>
                <member><name>kept</name><value><string>yes</string></value></member>
            </struct></value></param>
        </params></methodCall>"#;

        let request = parse_request(xml.as_bytes()).expect("well-formed XML should parse");
        assert_eq!(request.params.len(), 2);
        assert_eq!(request.params[0].as_str(), Some("kept"));
        let members = request.params[1].as_struct().expect("struct value");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name(), "kept");
        assert_eq!(members[0].value().as_str(), Some("yes"));
    }

    #[test]
    fn test_empty_implicit_value_is_omitted_like_original() {
        let xml = r#"<methodCall><methodName>aria2.addUri</methodName><params>
            <param><value></value></param>
            <param><value>   </value></param>
        </params></methodCall>"#;

        let request = parse_request(xml.as_bytes()).expect("well-formed XML should parse");
        assert_eq!(request.params.len(), 1);
        assert_eq!(request.params[0].as_str(), Some("   "));
    }

    #[test]
    fn test_array_response_is_one_xmlrpc_param() {
        let response = XmlRpcResponse::array_val(vec![XmlRpcValue::int(1), XmlRpcValue::int(2)]);
        let xml = response.to_xml();

        assert_eq!(xml.matches("<param>").count(), 1);
        assert!(xml.contains("<array>"));
        assert!(xml.contains("<int>1</int>"));
        assert!(xml.contains("<int>2</int>"));
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
