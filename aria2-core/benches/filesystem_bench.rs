use criterion::{criterion_group, Criterion, BenchmarkId, black_box};
use std::io::Write;
use tempfile::TempDir;
use base64::Engine;

fn bench_disk_write_sequential_10mb(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bench_write_10mb.bin");
    let data: Vec<u8> = (0..(10 * 1024 * 1024)).map(|i| (i % 256) as u8).collect();

    c.bench_with_input(BenchmarkId::new("disk_write_sequential_10MB", 10), &data, |b, d| {
        b.iter(|| {
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(d).unwrap();
            file.sync_all().unwrap();
            black_box(d.len());
        });
    });
    let _ = std::fs::remove_file(&path);
}

fn bench_disk_read_sequential_10mb(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bench_read_10mb.bin");
    let data: Vec<u8> = (0..(10 * 1024 * 1024)).map(|i| (i % 256) as u8).collect();
    std::fs::write(&path, &data).unwrap();

    c.bench_function("disk_read_sequential_10MB", |b| {
        b.iter(|| {
            let buf = std::fs::read(&path).unwrap();
            black_box(buf.len());
        });
    });

    let _ = std::fs::remove_file(&path);
}

fn bench_base64_roundtrip_1mb(c: &mut Criterion) {
    let data: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 256) as u8).collect();

    c.bench_with_input(BenchmarkId::new("base64_roundtrip_1MB", 1), &data, |b, d| {
        b.iter(|| {
            let encoded = base64::engine::general_purpose::STANDARD.encode(d);
            let decoded = base64::engine::general_purpose::STANDARD.decode(&encoded).unwrap();
            black_box(decoded.len());
        });
    });
}

fn bench_json_serialize_10kb(c: &mut Criterion) {
    let obj = serde_json::json!({
        "gid": "abc123def456",
        "totalLength": 104857600,
        "completedLength": 52428800,
        "downloadSpeed": 12595200,
        "uploadSpeed": 2048000,
        "status": "active",
        "files": [
            {"index": 0, "path": "/downloads/file.iso", "length": 104857600}
        ]
    });

    c.bench_function("json_serialize_10KB_object", |b| {
        b.iter(|| {
            let s = serde_json::to_string(&obj);
            black_box(s.ok());
        });
    });
}

fn bench_json_parse_10kb(c: &mut Criterion) {
    let json_str: String = serde_json::to_string(&serde_json::json!({
        "gid": "abc123def456",
        "totalLength": 104857600,
        "completedLength": 52428800,
        "downloadSpeed": 12595200,
        "uploadSpeed": 2048000,
        "status": "active",
        "files": [
            {"index": 0, "path": "/downloads/file.iso", "length": 104857600},
            {"index": 1, "path": "/downloads/data.bin", "length": 52428800}
        ]
    })).unwrap();

    c.bench_function("json_parse_10KB_string", |b| {
        b.iter(|| {
            let val: serde_json::Value = serde_json::from_str(&json_str).unwrap();
            black_box(val["gid"].as_str().map(|s| s.len()).unwrap_or(0));
        });
    });
}

fn bench_path_operations(c: &mut Criterion) {
    let paths: Vec<std::path::PathBuf> = (0..100)
        .map(|i| std::path::PathBuf::from(format!("/some/deep/path/{}/file{}.txt", i / 25, i)))
        .collect();

    c.bench_with_input(BenchmarkId::new("path_operations_100_paths", 100), &paths, |b, ps| {
        b.iter(|| {
            let mut total_len = 0usize;
            for p in ps.iter() {
                total_len += p.file_name().map_or(0, |n| n.len());
                total_len += p.parent().map_or(0, |d| d.display().to_string().len());
                total_len += p.extension().map_or(0, |e| e.len());
            }
            black_box(total_len);
        });
    });
}

fn bench_string_concat(c: &mut Criterion) {
    let parts: Vec<String> = (0..50).map(|i| format!("part{}_of_string", i)).collect();

    c.bench_with_input(BenchmarkId::new("string_concat_50_parts", 50), &parts, |b, ps| {
        b.iter(|| {
            let result: String = ps.concat();
            black_box(result.len());
        });
    });
}

fn bench_hashmap_insert_lookup(c: &mut Criterion) {
    c.bench_function("hashmap_insert_lookup_1000_ops", |b| {
        b.iter(|| {
            let mut map = std::collections::HashMap::new();
            for i in 0..1000 {
                map.insert(format!("key{}", i), format!("val{}", i));
            }
            let mut hits = 0;
            for i in 0..1000 {
                if map.contains_key(&format!("key{}", i)) { hits += 1; }
            }
            black_box(hits);
        });
    });
}

criterion_group!(filesystem_benches,
    bench_disk_write_sequential_10mb,
    bench_disk_read_sequential_10mb,
    bench_base64_roundtrip_1mb,
    bench_json_serialize_10kb,
    bench_json_parse_10kb,
    bench_path_operations,
    bench_string_concat,
    bench_hashmap_insert_lookup,
);

fn main() {
    filesystem_benches();
}
