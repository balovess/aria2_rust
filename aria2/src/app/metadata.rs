//! Metadata-only CLI actions.
//!
//! These actions mirror aria2_original's `--show-files` path: metadata is
//! parsed and printed before the download engine is initialized.

#[cfg(feature = "metalink")]
use aria2_core::util::format::format_bytes;
use aria2_core::validation::protocol_detector::DetectedInput;
#[cfg(any(feature = "bittorrent", feature = "metalink"))]
use aria2_core::validation::protocol_detector::InputType;

pub(super) fn show_files(inputs: &[DetectedInput]) -> Result<(), String> {
    for input in inputs {
        println!(">>> {}", input.raw);
        match &input.input_type {
            #[cfg(feature = "bittorrent")]
            InputType::TorrentFile => {
                let data = input
                    .file_data
                    .as_deref()
                    .ok_or_else(|| format!("Torrent file data not available: {}", input.raw))?;
                show_torrent(data)?;
            }
            #[cfg(feature = "metalink")]
            InputType::MetalinkFile => {
                let data = input
                    .file_data
                    .as_deref()
                    .ok_or_else(|| format!("Metalink file data not available: {}", input.raw))?;
                show_metalink(data)?;
            }
            _ => println!("Not a torrent or metalink file\n"),
        }
    }
    Ok(())
}

#[cfg(feature = "bittorrent")]
fn show_torrent(data: &[u8]) -> Result<(), String> {
    use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;

    let torrent = TorrentMeta::parse(data)?;
    println!("{}", render_torrent(&torrent));
    Ok(())
}

#[cfg(feature = "bittorrent")]
fn render_torrent(torrent: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta) -> String {
    use std::fmt::Write;

    let mut output = String::new();
    writeln!(output, "*** BitTorrent File Information ***").unwrap();
    if let Some(comment) = torrent.comment.as_deref() {
        writeln!(output, "Comment: {comment}").unwrap();
    }
    if let Some(creation_date) = torrent.creation_date.filter(|date| *date != 0) {
        writeln!(
            output,
            "Creation Date: {}",
            aria2_core::http::cookie::parsing::format_http_date(creation_date)
        )
        .unwrap();
    }
    if let Some(created_by) = torrent.created_by.as_deref() {
        writeln!(output, "Created By: {created_by}").unwrap();
    }
    writeln!(
        output,
        "Mode: {}",
        if torrent.info.length.is_some() {
            "single"
        } else {
            "multi"
        }
    )
    .unwrap();
    writeln!(output, "Announce:").unwrap();
    if torrent.announce_list.is_empty() {
        writeln!(output, " {}", torrent.announce).unwrap();
    } else {
        for tier in &torrent.announce_list {
            writeln!(output, " {}", tier.join(" ")).unwrap();
        }
    }
    writeln!(output, "Info Hash: {}", torrent.info_hash.as_hex()).unwrap();
    writeln!(
        output,
        "Piece Length: {}B",
        format_abbrev_bytes(u64::from(torrent.info.piece_length))
    )
    .unwrap();
    writeln!(
        output,
        "The Number of Pieces: {}",
        torrent.info.pieces.len()
    )
    .unwrap();
    writeln!(
        output,
        "Total Length: {}B ({})",
        format_abbrev_bytes(torrent.total_size()),
        format_comma(torrent.total_size())
    )
    .unwrap();
    if !torrent.web_seeds.is_empty() {
        writeln!(output, "URL List:").unwrap();
        for url in &torrent.web_seeds {
            writeln!(output, " {url}").unwrap();
        }
    }
    if !torrent.nodes.is_empty() {
        writeln!(output, "Nodes:").unwrap();
        for (host, port) in &torrent.nodes {
            writeln!(output, " {host}:{port}").unwrap();
        }
    }
    writeln!(output, "Name: {}", torrent.info.name).unwrap();
    writeln!(output, "Magnet URI: {}", torrent_magnet_uri(torrent)).unwrap();
    writeln!(output, "Files:").unwrap();
    writeln!(output, "idx|path/length").unwrap();
    writeln!(
        output,
        "===+=============================================================="
    )
    .unwrap();
    if let Some(length) = torrent.info.length {
        write_file_row(&mut output, 1, &torrent.info.name, length);
    } else if let Some(files) = torrent.info.files.as_ref() {
        for (index, file) in files.iter().enumerate() {
            write_file_row(&mut output, index + 1, &file.path.join("/"), file.length);
        }
    }
    output
}

#[cfg(feature = "bittorrent")]
fn write_file_row(output: &mut String, index: usize, path: &str, length: u64) {
    use std::fmt::Write;

    writeln!(output, "{index:>3}|{path}").unwrap();
    writeln!(
        output,
        "   |{}B ({})",
        format_abbrev_bytes(length),
        format_comma(length)
    )
    .unwrap();
    writeln!(
        output,
        "---+------------------------------------------------------------"
    )
    .unwrap();
}

#[cfg(feature = "metalink")]
fn print_file_row(index: usize, path: &str, length: u64) {
    println!("{index:>3}|{path}");
    println!("   |{} ({length} B)", format_bytes(length));
    println!("---+------------------------------------------------------------");
}

#[cfg(feature = "bittorrent")]
fn percent_encode_query_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(feature = "bittorrent")]
fn torrent_magnet_uri(
    torrent: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
) -> String {
    let mut uri = format!(
        "magnet:?xt=urn:btih:{}",
        torrent.info_hash.as_hex().to_ascii_uppercase()
    );
    if !torrent.info.name.is_empty() {
        uri.push_str("&dn=");
        uri.push_str(&percent_encode_query_component(&torrent.info.name));
    }
    if torrent.announce_list.is_empty() {
        if !torrent.announce.is_empty() {
            uri.push_str("&tr=");
            uri.push_str(&percent_encode_query_component(&torrent.announce));
        }
    } else {
        for tier in &torrent.announce_list {
            for tracker in tier {
                uri.push_str("&tr=");
                uri.push_str(&percent_encode_query_component(tracker));
            }
        }
    }
    uri
}

#[cfg(feature = "bittorrent")]
fn format_abbrev_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["", "Ki", "Mi", "Gi"];
    let mut value = bytes;
    let mut remainder = 0;
    let mut unit = 0;
    while value >= 1024 && unit + 1 < UNITS.len() {
        remainder = value % 1024;
        value /= 1024;
        unit += 1;
    }
    if unit + 1 < UNITS.len() && value >= 922 {
        remainder = value;
        value = 0;
        unit += 1;
    }
    let fractional = (value < 10 && unit > 0).then(|| remainder * 10 / 1024);
    match fractional {
        Some(decimal) => format!("{value}.{decimal}{}", UNITS[unit]),
        None => format!("{value}{}", UNITS[unit]),
    }
}

#[cfg(feature = "bittorrent")]
fn format_comma(value: u64) -> String {
    let digits = value.to_string();
    let first_group = digits.len() % 3;
    let first_group = if first_group == 0 { 3 } else { first_group };
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    output.push_str(&digits[..first_group]);
    for chunk in digits[first_group..].as_bytes().chunks(3) {
        output.push(',');
        output.push_str(std::str::from_utf8(chunk).expect("digits are valid UTF-8"));
    }
    output
}

#[cfg(feature = "metalink")]
fn show_metalink(data: &[u8]) -> Result<(), String> {
    use aria2_protocol::metalink::parser::MetalinkDocument;

    let document = MetalinkDocument::parse(data, None).map_err(|error| error.to_string())?;
    println!("*** Metalink File Information ***");
    println!("Files:");
    println!("idx|path/length");
    println!("===+==============================================================");
    for (index, file) in document.files.iter().enumerate() {
        print_file_row(index + 1, &file.name, file.size.unwrap_or(0));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "bittorrent")]
    #[test]
    fn show_torrent_contains_original_metadata_sections() {
        let data = b"d8:announce14:http://tracker4:infod4:name8:test.bin6:lengthi4e12:piece lengthi4e6:pieces20:12345678901234567890ee";
        let torrent = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(data)
            .expect("test torrent should parse");
        let output = super::render_torrent(&torrent);
        assert!(output.contains("*** BitTorrent File Information ***"));
        assert!(output.contains("Info Hash:"));
        assert!(output.contains("Files:"));
        assert!(output.contains("test.bin"));
    }
}
