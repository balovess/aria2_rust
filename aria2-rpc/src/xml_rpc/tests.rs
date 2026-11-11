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
fn test_parse_without_method_name_keeps_aria2_dispatch_shape() {
    let xml = r#"<methodCall><params></params></methodCall>"#;
    let request = parse_request(xml.as_bytes()).expect("aria2 accepts an empty method name");
    assert!(request.method_name.is_empty());
    assert!(request.params.is_empty());
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
