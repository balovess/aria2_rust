//! Metadata-only CLI actions.
//!
//! These actions mirror aria2_original's `--show-files` path: metadata is
//! parsed and printed before the download engine is initialized.

use aria2_core::util::format::format_bytes;
use aria2_core::validation::protocol_detector::{DetectedInput, InputType};

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
        "Piece Length: {}",
        format_bytes(u64::from(torrent.info.piece_length))
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
        "Total Length: {} ({})",
        format_bytes(torrent.total_size()),
        torrent.total_size()
    )
    .unwrap();
    if !torrent.web_seeds.is_empty() {
        writeln!(output, "URL List:").unwrap();
        for url in &torrent.web_seeds {
            writeln!(output, " {url}").unwrap();
        }
    }
    writeln!(output, "Name: {}", torrent.info.name).unwrap();
    writeln!(
        output,
        "Magnet URI: magnet:?xt=urn:btih:{}&dn={}",
        torrent.info_hash.as_hex(),
        percent_encode_query_component(&torrent.info.name)
    )
    .unwrap();
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
    writeln!(output, "   |{} ({length} B)", format_bytes(length)).unwrap();
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
