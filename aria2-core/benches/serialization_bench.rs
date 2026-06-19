//! Serialization Performance Benchmark
//!
//! This benchmark measures the performance of various serialization operations:
//! - Session JSON serialization efficiency
//! - DHT bencode encoding efficiency
//! - Config file parsing efficiency
//!
//! The goal is to identify serialization bottlenecks and provide optimization recommendations.

use criterion::{criterion_group, Criterion, black_box};
use std::collections::HashMap;
use std::sync::Arc;

// ==================== Session Serialization Benchmarks ====================

fn gen_session_entry(n_uris: usize, n_options: usize) -> aria2_core::session::session_entry::SessionEntry {
    let uris: Vec<String> = (0..n_uris)
        .map(|i| format!("http://mirror{}.example.com/large-file-{}.iso", i, i))
        .collect();

    let options: HashMap<String, String> = (0..n_options)
        .map(|i| (format!("option{}", i), format!("value{}", i)))
        .collect();

    let mut entry = aria2_core::session::session_entry::SessionEntry::new(0x123456789ABCDEF, uris);
    entry.options = options;
    entry.total_length = 1024 * 1024 * 1024; // 1GB
    entry.completed_length = 512 * 1024 * 1024; // 512MB
    entry.upload_length = 1024 * 1024; // 1MB
    entry.download_speed = 2048 * 1024; // 2MB/s
    entry.status = "active".to_string();
    entry
}

fn gen_session_entries(n_entries: usize) -> Vec<aria2_core::session::session_entry::SessionEntry> {
    (0..n_entries)
        .map(|i| {
            let mut entry = gen_session_entry(3, 10);
            entry.gid = i as u64;
            entry
        })
        .collect()
}

fn bench_session_entry_serialize(c: &mut Criterion) {
    let entry_small = gen_session_entry(1, 5);
    let entry_medium = gen_session_entry(5, 20);
    let entry_large = gen_session_entry(20, 50);

    c.bench_function("session_serialize_small_1uri_5opts", |b| {
        b.iter(|| black_box(entry_small.serialize()))
    });

    c.bench_function("session_serialize_medium_5uris_20opts", |b| {
        b.iter(|| black_box(entry_medium.serialize()))
    });

    c.bench_function("session_serialize_large_20uris_50opts", |b| {
        b.iter(|| black_box(entry_large.serialize()))
    });
}

fn bench_session_entry_deserialize(c: &mut Criterion) {
    let entry_small = gen_session_entry(1, 5);
    let entry_medium = gen_session_entry(5, 20);
    let entry_large = gen_session_entry(20, 50);

    let serialized_small = entry_small.serialize();
    let serialized_medium = entry_medium.serialize();
    let serialized_large = entry_large.serialize();

    c.bench_function("session_deserialize_small", |b| {
        b.iter(|| {
            let entry = aria2_core::session::session_entry::SessionEntry::deserialize_line(&serialized_small);
            black_box(entry.is_ok())
        })
    });

    c.bench_function("session_deserialize_medium", |b| {
        b.iter(|| {
            let entry = aria2_core::session::session_entry::SessionEntry::deserialize_line(&serialized_medium);
            black_box(entry.is_ok())
        })
    });

    c.bench_function("session_deserialize_large", |b| {
        b.iter(|| {
            let entry = aria2_core::session::session_entry::SessionEntry::deserialize_line(&serialized_large);
            black_box(entry.is_ok())
        })
    });
}

fn bench_session_batch_serialize(c: &mut Criterion) {
    let entries_10 = gen_session_entries(10);
    let entries_50 = gen_session_entries(50);
    let entries_100 = gen_session_entries(100);

    c.bench_function("session_batch_serialize_10_entries", |b| {
        b.iter(|| {
            let mut output = String::new();
            for entry in &entries_10 {
                output.push_str(&entry.serialize());
                output.push('\n');
            }
            black_box(output.len())
        })
    });

    c.bench_function("session_batch_serialize_50_entries", |b| {
        b.iter(|| {
            let mut output = String::new();
            for entry in &entries_50 {
                output.push_str(&entry.serialize());
                output.push('\n');
            }
            black_box(output.len())
        })
    });

    c.bench_function("session_batch_serialize_100_entries", |b| {
        b.iter(|| {
            let mut output = String::new();
            for entry in &entries_100 {
                output.push_str(&entry.serialize());
                output.push('\n');
            }
            black_box(output.len())
        })
    });
}

fn bench_session_with_bitfield(c: &mut Criterion) {
    let mut entry = gen_session_entry(3, 10);
    // Simulate BitTorrent download with large bitfield
    entry.bitfield = Some((0..10000).map(|i| (i % 256) as u8).collect());
    entry.num_pieces = Some(80000); // 10000 bytes * 8 bits
    entry.piece_length = Some(262144); // 256KB pieces
    entry.info_hash_hex = Some("abc123def456789012345678901234567890abcd".to_string());

    c.bench_function("session_serialize_with_large_bitfield", |b| {
        b.iter(|| black_box(entry.serialize()))
    });

    let serialized = entry.serialize();
    c.bench_function("session_deserialize_with_large_bitfield", |b| {
        b.iter(|| {
            let result = aria2_core::session::session_entry::SessionEntry::deserialize_line(&serialized);
            black_box(result.is_ok())
        })
    });
}

// ==================== Bencode Serialization Benchmarks ====================

fn gen_bencode_dict(n_keys: usize) -> aria2_protocol::bittorrent::bencode::codec::BencodeValue {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    let mut dict = BTreeMap::new();
    for i in 0..n_keys {
        dict.insert(
            format!("key{}", i).into_bytes(),
            BencodeValue::Int(i as i64),
        );
    }
    BencodeValue::Dict(dict)
}

fn gen_bencode_nested_dict(depth: usize, width: usize) -> aria2_protocol::bittorrent::bencode::codec::BencodeValue {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    if depth == 0 {
        return BencodeValue::Int(42);
    }

    let mut dict = BTreeMap::new();
    for i in 0..width {
        let key = format!("level{}_key{}", depth, i).into_bytes();
        let value = gen_bencode_nested_dict(depth - 1, width);
        dict.insert(key, value);
    }
    BencodeValue::Dict(dict)
}

fn gen_bencode_list(n_items: usize) -> aria2_protocol::bittorrent::bencode::codec::BencodeValue {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let items: Vec<BencodeValue> = (0..n_items)
        .map(|i| BencodeValue::Int(i as i64))
        .collect();
    BencodeValue::List(items)
}

fn bench_bencode_encode(c: &mut Criterion) {
    let dict_small = gen_bencode_dict(10);
    let dict_medium = gen_bencode_dict(50);
    let dict_large = gen_bencode_dict(200);

    c.bench_function("bencode_encode_dict_10_keys", |b| {
        b.iter(|| black_box(dict_small.encode().len()))
    });

    c.bench_function("bencode_encode_dict_50_keys", |b| {
        b.iter(|| black_box(dict_medium.encode().len()))
    });

    c.bench_function("bencode_encode_dict_200_keys", |b| {
        b.iter(|| black_box(dict_large.encode().len()))
    });

    let list_small = gen_bencode_list(20);
    let list_medium = gen_bencode_list(100);
    let list_large = gen_bencode_list(500);

    c.bench_function("bencode_encode_list_20_items", |b| {
        b.iter(|| black_box(list_small.encode().len()))
    });

    c.bench_function("bencode_encode_list_100_items", |b| {
        b.iter(|| black_box(list_medium.encode().len()))
    });

    c.bench_function("bencode_encode_list_500_items", |b| {
        b.iter(|| black_box(list_large.encode().len()))
    });
}

fn bench_bencode_decode(c: &mut Criterion) {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let dict_small = gen_bencode_dict(10);
    let dict_medium = gen_bencode_dict(50);
    let dict_large = gen_bencode_dict(200);

    let encoded_small = dict_small.encode();
    let encoded_medium = dict_medium.encode();
    let encoded_large = dict_large.encode();

    c.bench_function("bencode_decode_dict_10_keys", |b| {
        b.iter(|| black_box(BencodeValue::decode(&encoded_small).is_ok()))
    });

    c.bench_function("bencode_decode_dict_50_keys", |b| {
        b.iter(|| black_box(BencodeValue::decode(&encoded_medium).is_ok()))
    });

    c.bench_function("bencode_decode_dict_200_keys", |b| {
        b.iter(|| black_box(BencodeValue::decode(&encoded_large).is_ok()))
    });
}

fn bench_bencode_nested_structures(c: &mut Criterion) {
    let nested_shallow = gen_bencode_nested_dict(2, 5);
    let nested_medium = gen_bencode_nested_dict(4, 5);
    let nested_deep = gen_bencode_nested_dict(6, 5);

    c.bench_function("bencode_encode_nested_depth2_width5", |b| {
        b.iter(|| black_box(nested_shallow.encode().len()))
    });

    c.bench_function("bencode_encode_nested_depth4_width5", |b| {
        b.iter(|| black_box(nested_medium.encode().len()))
    });

    c.bench_function("bencode_encode_nested_depth6_width5", |b| {
        b.iter(|| black_box(nested_deep.encode().len()))
    });
}

fn bench_bencode_bytes_operations(c: &mut Criterion) {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    // Test with different byte array sizes
    let bytes_1kb = BencodeValue::Bytes((0..1024).map(|i| (i % 256) as u8).collect());
    let bytes_10kb = BencodeValue::Bytes((0..10240).map(|i| (i % 256) as u8).collect());
    let bytes_100kb = BencodeValue::Bytes((0..102400).map(|i| (i % 256) as u8).collect());

    c.bench_function("bencode_encode_bytes_1KB", |b| {
        b.iter(|| black_box(bytes_1kb.encode().len()))
    });

    c.bench_function("bencode_encode_bytes_10KB", |b| {
        b.iter(|| black_box(bytes_10kb.encode().len()))
    });

    c.bench_function("bencode_encode_bytes_100KB", |b| {
        b.iter(|| black_box(bytes_100kb.encode().len()))
    });

    let encoded_1kb = bytes_1kb.encode();
    let encoded_10kb = bytes_10kb.encode();
    let encoded_100kb = bytes_100kb.encode();

    c.bench_function("bencode_decode_bytes_1KB", |b| {
        b.iter(|| black_box(BencodeValue::decode(&encoded_1kb).is_ok()))
    });

    c.bench_function("bencode_decode_bytes_10KB", |b| {
        b.iter(|| black_box(BencodeValue::decode(&encoded_10kb).is_ok()))
    });

    c.bench_function("bencode_decode_bytes_100KB", |b| {
        b.iter(|| black_box(BencodeValue::decode(&encoded_100kb).is_ok()))
    });
}

// ==================== Config Parsing Benchmarks ====================

fn gen_config_content(n_options: usize) -> String {
    let mut content = String::new();
    content.push_str("# Configuration file for aria2-rust\n");
    content.push_str("# Generated for benchmarking\n\n");

    for i in 0..n_options {
        content.push_str(&format!("option{}=value{}\n", i, i));
    }

    content
}

fn gen_cli_args(n_args: usize) -> Vec<String> {
    (0..n_args)
        .map(|i| format!("--option{}=value{}", i, i))
        .collect()
}

fn bench_config_parser(c: &mut Criterion) {
    use aria2_core::config::parser::ConfigParser;

    let content_small = gen_config_content(20);
    let content_medium = gen_config_content(100);
    let content_large = gen_config_content(500);

    c.bench_function("config_parse_file_20_options", |b| {
        b.iter(|| {
            let mut parser = ConfigParser::new();
            // Parse from string content instead
            for line in content_small.lines() {
                if let Some(eq_pos) = line.find('=') {
                    let name = line[..eq_pos].trim();
                    let value = line[eq_pos + 1..].trim();
                    if !name.is_empty() && !name.starts_with('#') {
                        parser.set_raw(name, value);
                    }
                }
            }
            black_box(parser.options().len())
        })
    });

    c.bench_function("config_parse_file_100_options", |b| {
        b.iter(|| {
            let mut parser = ConfigParser::new();
            for line in content_medium.lines() {
                if let Some(eq_pos) = line.find('=') {
                    let name = line[..eq_pos].trim();
                    let value = line[eq_pos + 1..].trim();
                    if !name.is_empty() && !name.starts_with('#') {
                        parser.set_raw(name, value);
                    }
                }
            }
            black_box(parser.options().len())
        })
    });

    c.bench_function("config_parse_file_500_options", |b| {
        b.iter(|| {
            let mut parser = ConfigParser::new();
            for line in content_large.lines() {
                if let Some(eq_pos) = line.find('=') {
                    let name = line[..eq_pos].trim();
                    let value = line[eq_pos + 1..].trim();
                    if !name.is_empty() && !name.starts_with('#') {
                        parser.set_raw(name, value);
                    }
                }
            }
            black_box(parser.options().len())
        })
    });
}

fn bench_cli_args_parsing(c: &mut Criterion) {
    use aria2_core::config::parser::ConfigParser;

    let args_small = gen_cli_args(10);
    let args_small_refs: Vec<&str> = args_small.iter().map(|s| s.as_str()).collect();
    let args_medium = gen_cli_args(50);
    let args_medium_refs: Vec<&str> = args_medium.iter().map(|s| s.as_str()).collect();
    let args_large = gen_cli_args(200);
    let args_large_refs: Vec<&str> = args_large.iter().map(|s| s.as_str()).collect();

    c.bench_function("config_parse_cli_10_args", |b| {
        b.iter(|| {
            let mut parser = ConfigParser::new();
            parser.parse_cli_args(&args_small_refs);
            black_box(parser.options().len())
        })
    });

    c.bench_function("config_parse_cli_50_args", |b| {
        b.iter(|| {
            let mut parser = ConfigParser::new();
            parser.parse_cli_args(&args_medium_refs);
            black_box(parser.options().len())
        })
    });

    c.bench_function("config_parse_cli_200_args", |b| {
        b.iter(|| {
            let mut parser = ConfigParser::new();
            parser.parse_cli_args(&args_large_refs);
            black_box(parser.options().len())
        })
    });
}

// ==================== JSON Serialization Benchmarks ====================

fn gen_json_value(n_keys: usize) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for i in 0..n_keys {
        map.insert(
            format!("key{}", i),
            serde_json::json!({
                "nested": {
                    "value": i,
                    "data": format!("string_value_{}", i)
                }
            }),
        );
    }
    serde_json::Value::Object(map)
}

fn bench_json_serialization(c: &mut Criterion) {
    let value_small = gen_json_value(10);
    let value_medium = gen_json_value(50);
    let value_large = gen_json_value(200);

    c.bench_function("json_serialize_10_keys", |b| {
        b.iter(|| black_box(serde_json::to_string(&value_small).unwrap().len()))
    });

    c.bench_function("json_serialize_50_keys", |b| {
        b.iter(|| black_box(serde_json::to_string(&value_medium).unwrap().len()))
    });

    c.bench_function("json_serialize_200_keys", |b| {
        b.iter(|| black_box(serde_json::to_string(&value_large).unwrap().len()))
    });

    let json_small = serde_json::to_string(&value_small).unwrap();
    let json_medium = serde_json::to_string(&value_medium).unwrap();
    let json_large = serde_json::to_string(&value_large).unwrap();

    c.bench_function("json_deserialize_10_keys", |b| {
        b.iter(|| {
            let v: Result<serde_json::Value, _> = serde_json::from_str(&json_small);
            black_box(v.is_ok())
        })
    });

    c.bench_function("json_deserialize_50_keys", |b| {
        b.iter(|| {
            let v: Result<serde_json::Value, _> = serde_json::from_str(&json_medium);
            black_box(v.is_ok())
        })
    });

    c.bench_function("json_deserialize_200_keys", |b| {
        b.iter(|| {
            let v: Result<serde_json::Value, _> = serde_json::from_str(&json_large);
            black_box(v.is_ok())
        })
    });
}

// ==================== Performance Monitor Integration ====================

fn bench_perf_monitor_overhead(c: &mut Criterion) {
    use aria2_core::util::perf_monitor::{Metrics, PerformanceMonitor};

    let monitor = Arc::new(PerformanceMonitor::new());

    c.bench_function("perf_monitor_record_metric", |b| {
        b.iter(|| {
            let metrics = Metrics::new(1000, 50, 1024, 10);
            monitor.record_metric("test", metrics);
            black_box(())
        })
    });

    c.bench_function("perf_monitor_generate_report", |b| {
        b.iter(|| black_box(monitor.generate_report().summary.total_samples))
    });

    c.bench_function("perf_monitor_export_json", |b| {
        b.iter(|| black_box(monitor.export_json().len()))
    });
}

criterion_group!(
    serialization_benches,
    // Session serialization
    bench_session_entry_serialize,
    bench_session_entry_deserialize,
    bench_session_batch_serialize,
    bench_session_with_bitfield,
    // Bencode serialization
    bench_bencode_encode,
    bench_bencode_decode,
    bench_bencode_nested_structures,
    bench_bencode_bytes_operations,
    // Config parsing
    bench_config_parser,
    bench_cli_args_parsing,
    // JSON serialization
    bench_json_serialization,
    // Performance monitor
    bench_perf_monitor_overhead,
);

fn main() {
    serialization_benches();
}
