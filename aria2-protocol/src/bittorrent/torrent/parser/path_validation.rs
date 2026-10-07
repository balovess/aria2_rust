pub(super) fn validate_components(components: &[String]) -> Result<(), String> {
    if components.is_empty() {
        return Err("file path is empty".to_string());
    }
    for component in components {
        let bytes = component.as_bytes();
        let has_windows_drive_prefix =
            bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('/')
            || component.contains('\\')
            || has_windows_drive_prefix
            || component.chars().any(char::is_control)
        {
            return Err(format!("unsafe torrent path component: {component:?}"));
        }
    }
    Ok(())
}

pub(super) fn decode_component(bytes: &[u8]) -> String {
    if let Ok(component) = std::str::from_utf8(bytes) {
        return component.to_owned();
    }

    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}
