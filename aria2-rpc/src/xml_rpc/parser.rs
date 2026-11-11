use super::{XmlRpcError, XmlRpcMember, XmlRpcRequest, XmlRpcValue, XmlRpcValueInner};

fn xml_tag(e: &quick_xml::events::BytesStart<'_>) -> Result<String, XmlRpcError> {
    Ok(e.local_name().as_ref().to_owned())
}

fn read_xml_text(
    reader: &mut quick_xml::Reader<&[u8]>,
    end: quick_xml::name::QName<'_>,
) -> Result<String, XmlRpcError> {
    let text = reader
        .read_text(end)
        .map_err(|error| XmlRpcError::ParseError(error.to_string()))?;
    let decoded = text.xml10_content();
    quick_xml::escape::unescape(&decoded)
        .map(std::borrow::Cow::into_owned)
        .map_err(|error| XmlRpcError::ParseError(error.to_string()))
}

fn read_scalar(
    reader: &mut quick_xml::Reader<&[u8]>,
    e: &quick_xml::events::BytesStart<'_>,
    tag: &str,
) -> Result<Option<XmlRpcValue>, XmlRpcError> {
    let text = read_xml_text(reader, e.name())?;
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
                let decoded = text.xml10_content();
                let text = quick_xml::escape::unescape(&decoded)
                    .map_err(|error| XmlRpcError::ParseError(error.to_string()))?;
                implicit_text.push_str(&text);
            }
            Event::CData(text) => {
                implicit_text.push_str(&text.xml10_content());
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
                            name = Some(read_xml_text(reader, e.name())?);
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
                method_name = Some(read_xml_text(&mut reader, e.name())?);
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
