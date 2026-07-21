//! Simple Serialization Performance Test
//!
//! This test measures serialization performance without requiring criterion framework.

use std::time::{Duration, Instant};

fn measure<F: Fn()>(_name: &str, iterations: usize, f: F) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        f();
    }
    start.elapsed()
}

fn format_duration(d: Duration) -> String {
    if d.as_micros() < 1000 {
        format!("{} µs", d.as_micros())
    } else if d.as_millis() < 1000 {
        format!("{} ms", d.as_millis())
    } else {
        format!("{} s", d.as_secs())
    }
}

#[test]
fn test_serialization_performance() {
    println!("=== Serialization Performance Analysis ===\n");

    // Test Session Entry serialization
    println!("1. Session Entry Serialization Performance");
    println!("-------------------------------------------");

    // Small entry
    let uris_small: Vec<String> = vec!["http://example.com/file.zip".to_string()];
    let mut entry_small = aria2_core::session::session_entry::SessionEntry::new(1, uris_small);
    entry_small.total_length = 1024 * 1024;
    entry_small.completed_length = 512 * 1024;

    let time = measure("session_serialize_small", 10000, || {
        let _ = entry_small.serialize();
    });
    println!(
        "  Small entry (1 URI): {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    // Medium entry
    let uris_medium: Vec<String> = (0..5)
        .map(|i| format!("http://mirror{}.com/file.iso", i))
        .collect();
    let mut entry_medium = aria2_core::session::session_entry::SessionEntry::new(2, uris_medium);
    for i in 0..20 {
        entry_medium
            .options
            .insert(format!("opt{}", i), format!("val{}", i));
    }
    entry_medium.total_length = 1024 * 1024 * 1024;
    entry_medium.completed_length = 512 * 1024 * 1024;

    let time = measure("session_serialize_medium", 10000, || {
        let _ = entry_medium.serialize();
    });
    println!(
        "  Medium entry (5 URIs, 20 options): {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    // Large entry with bitfield
    let uris_large: Vec<String> = (0..20)
        .map(|i| format!("http://mirror{}.com/file.iso", i))
        .collect();
    let mut entry_large = aria2_core::session::session_entry::SessionEntry::new(3, uris_large);
    for i in 0..50 {
        entry_large
            .options
            .insert(format!("opt{}", i), format!("val{}", i));
    }
    entry_large.bitfield = Some((0..10000).map(|i| (i % 256) as u8).collect());
    entry_large.num_pieces = Some(80000);
    entry_large.piece_length = Some(262144);

    let time = measure("session_serialize_large", 1000, || {
        let _ = entry_large.serialize();
    });
    println!(
        "  Large entry (20 URIs, 50 options, 10KB bitfield): {} for 1,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 1000);

    // Test deserialization
    println!("\n2. Session Entry Deserialization Performance");
    println!("---------------------------------------------");

    let serialized_small = entry_small.serialize();
    let time = measure("session_deserialize_small", 10000, || {
        let _ =
            aria2_core::session::session_entry::SessionEntry::deserialize_line(&serialized_small);
    });
    println!(
        "  Small entry: {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    let serialized_medium = entry_medium.serialize();
    let time = measure("session_deserialize_medium", 10000, || {
        let _ =
            aria2_core::session::session_entry::SessionEntry::deserialize_line(&serialized_medium);
    });
    println!(
        "  Medium entry: {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    let serialized_large = entry_large.serialize();
    let time = measure("session_deserialize_large", 1000, || {
        let _ =
            aria2_core::session::session_entry::SessionEntry::deserialize_line(&serialized_large);
    });
    println!(
        "  Large entry: {} for 1,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 1000);

    // Test Bencode encoding
    println!("\n3. Bencode Encoding Performance");
    println!("---------------------------------");

    #[cfg(feature = "bittorrent")]
    {
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

    // Small dict
    let mut dict_small = BTreeMap::new();
    for i in 0..10 {
        dict_small.insert(
            format!("key{}", i).into_bytes(),
            BencodeValue::Int(i as i64),
        );
    }
    let bencode_small = BencodeValue::Dict(dict_small);

    let time = measure("bencode_encode_small", 10000, || {
        let _ = bencode_small.encode().len();
    });
    println!(
        "  Small dict (10 keys): {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    // Medium dict
    let mut dict_medium = BTreeMap::new();
    for i in 0..50 {
        dict_medium.insert(
            format!("key{}", i).into_bytes(),
            BencodeValue::Int(i as i64),
        );
    }
    let bencode_medium = BencodeValue::Dict(dict_medium);

    let time = measure("bencode_encode_medium", 10000, || {
        let _ = bencode_medium.encode().len();
    });
    println!(
        "  Medium dict (50 keys): {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    // Large dict
    let mut dict_large = BTreeMap::new();
    for i in 0..200 {
        dict_large.insert(
            format!("key{}", i).into_bytes(),
            BencodeValue::Int(i as i64),
        );
    }
    let bencode_large = BencodeValue::Dict(dict_large);

    let time = measure("bencode_encode_large", 1000, || {
        let _ = bencode_large.encode().len();
    });
    println!(
        "  Large dict (200 keys): {} for 1,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 1000);

    // Test Bencode decoding
    println!("\n4. Bencode Decoding Performance");
    println!("---------------------------------");

    let encoded_small = bencode_small.encode();
    let time = measure("bencode_decode_small", 10000, || {
        let _ = BencodeValue::decode(&encoded_small);
    });
    println!(
        "  Small dict: {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    let encoded_medium = bencode_medium.encode();
    let time = measure("bencode_decode_medium", 10000, || {
        let _ = BencodeValue::decode(&encoded_medium);
    });
    println!(
        "  Medium dict: {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    let encoded_large = bencode_large.encode();
    let time = measure("bencode_decode_large", 1000, || {
        let _ = BencodeValue::decode(&encoded_large);
    });
    println!(
        "  Large dict: {} for 1,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 1000);

    } // end #[cfg(feature = "bittorrent")]

    // Test Config parsing
    println!("\n5. Config Parsing Performance");
    println!("-------------------------------");

    use aria2_core::config::parser::ConfigParser;

    // Small config
    let config_small: String = (0..20)
        .map(|i| format!("option{}=value{}\n", i, i))
        .collect();

    let time = measure("config_parse_small", 10000, || {
        let mut parser = ConfigParser::new();
        for line in config_small.lines() {
            if let Some(eq_pos) = line.find('=') {
                let name = line[..eq_pos].trim();
                let value = line[eq_pos + 1..].trim();
                parser.set_raw(name, value);
            }
        }
    });
    println!(
        "  Small config (20 options): {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    // Medium config
    let config_medium: String = (0..100)
        .map(|i| format!("option{}=value{}\n", i, i))
        .collect();

    let time = measure("config_parse_medium", 10000, || {
        let mut parser = ConfigParser::new();
        for line in config_medium.lines() {
            if let Some(eq_pos) = line.find('=') {
                let name = line[..eq_pos].trim();
                let value = line[eq_pos + 1..].trim();
                parser.set_raw(name, value);
            }
        }
    });
    println!(
        "  Medium config (100 options): {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    // Large config
    let config_large: String = (0..500)
        .map(|i| format!("option{}=value{}\n", i, i))
        .collect();

    let time = measure("config_parse_large", 1000, || {
        let mut parser = ConfigParser::new();
        for line in config_large.lines() {
            if let Some(eq_pos) = line.find('=') {
                let name = line[..eq_pos].trim();
                let value = line[eq_pos + 1..].trim();
                parser.set_raw(name, value);
            }
        }
    });
    println!(
        "  Large config (500 options): {} for 1,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 1000);

    // Test JSON serialization
    println!("\n6. JSON Serialization Performance");
    println!("-----------------------------------");

    // Small JSON
    let mut json_small = serde_json::Map::new();
    for i in 0..10 {
        json_small.insert(format!("key{}", i), serde_json::json!(i));
    }
    let json_small_val = serde_json::Value::Object(json_small);

    let time = measure("json_serialize_small", 10000, || {
        let _ = serde_json::to_string(&json_small_val).unwrap().len();
    });
    println!(
        "  Small JSON (10 keys): {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    // Medium JSON
    let mut json_medium = serde_json::Map::new();
    for i in 0..50 {
        json_medium.insert(format!("key{}", i), serde_json::json!({"nested": i}));
    }
    let json_medium_val = serde_json::Value::Object(json_medium);

    let time = measure("json_serialize_medium", 10000, || {
        let _ = serde_json::to_string(&json_medium_val).unwrap().len();
    });
    println!(
        "  Medium JSON (50 keys): {} for 10,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 10000);

    // Large JSON
    let mut json_large = serde_json::Map::new();
    for i in 0..200 {
        json_large.insert(
            format!("key{}", i),
            serde_json::json!({"nested": {"value": i}}),
        );
    }
    let json_large_val = serde_json::Value::Object(json_large);

    let time = measure("json_serialize_large", 1000, || {
        let _ = serde_json::to_string(&json_large_val).unwrap().len();
    });
    println!(
        "  Large JSON (200 keys): {} for 1,000 iterations",
        format_duration(time)
    );
    println!("    Per operation: {:?}", time / 1000);

    println!("\n=== Performance Analysis Complete ===");
}
