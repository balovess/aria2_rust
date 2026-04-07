use criterion::{criterion_group, Criterion, BenchmarkId, black_box};
use aria2_core::config::{ConfigManager, OptionRegistry, OptionValue, OptionType, OptionCategory, OptionDef};
use aria2_core::config::parser::ConfigParser;
use aria2_core::config::uri_list::UriListFile;
use aria2_core::config::netrc::NetRcFile;

fn gen_registry() -> OptionRegistry {
    let reg = OptionRegistry::new();
    reg
}

fn gen_size_strings(n: usize) -> Vec<String> {
    let suffixes = ["B", "K", "M", "G", "T"];
    (0..n).map(|i| format!("{}{}", (i + 1) * 10, suffixes[i % 5])).collect()
}

fn gen_cli_args(n: usize) -> Vec<String> {
    let opts = ["dir=/tmp", "split=8", "max-tries=5", "timeout=60", "quiet=true",
        "max-download-limit=0", "max-upload-limit=0", "check-certificate=true"];
    (0..n).map(|i| format!("--{}", opts[i % opts.len()])).collect()
}

fn gen_uri_list_content(n: usize) -> String {
    (0..n).map(|i| format!("http://example.com/file{}.iso\thttp://mirror{}.com/file{}.iso\n", i, i, i)).collect()
}

fn gen_netrc_content(n: usize) -> String {
    (0..n).map(|i| format!(
        "machine host{}.example.com\nlogin user{}\npassword pass{}\n", i, i, i
    )).collect()
}

fn bench_registry_lookup(c: &mut Criterion) {
    let reg = gen_registry();
    c.bench_function("registry_lookup", |b| {
        b.iter(|| black_box(reg.get("split")));
    });
}

fn bench_registry_iterate(c: &mut Criterion) {
    let reg = gen_registry();
    c.bench_function("registry_iterate_all", |b| {
        b.iter(|| {
            let count = black_box(reg.all().values().count());
            std::hint::black_box(count);
        });
    });
}

fn bench_registry_contains(c: &mut Criterion) {
    let reg = gen_registry();
    c.bench_function("registry_contains", |b| {
        b.iter(|| {
            let r1 = black_box(reg.contains("split"));
            let r2 = black_box(reg.contains("nonexistent"));
            std::hint::black_box(r1 || r2);
        });
    });
}

fn bench_parse_size_str(c: &mut Criterion) {
    let sizes = gen_size_strings(100);
    c.bench_function("parse_size_str_100_values", |b| {
        b.iter_with_large_input(&sizes, |sizes| {
            for s in sizes.iter() {
                let v = aria2_core::config::option::OptionValue::parse_size_str(s);
                std::hint::black_box(v);
            }
        });
    });
}

fn bench_format_size(c: &mut Criterion) {
    let values: Vec<u64> = (0..100).map(|i| (i + 1) * 1024).collect();
    c.bench_function("format_size_100_values", |b| {
        b.iter_with_large_input(&values, |vals| {
            for v in vals.iter() {
                let s = aria2_core::ui::format_size(*v);
                std::hint::black_box(s);
            }
        });
    });
}

fn bench_parse_cli_args(c: &mut Criterion) {
    let args = gen_cli_args(50);
    let registry = gen_registry();
    c.bench(BenchmarkId::new("parse_cli_args_50_args"), |b| {
        b.iter_with_large_input(&args, |args| {
            let mut parser = ConfigParser::with_registry(registry.clone());
            parser.parse_cli_args(args);
            black_box(parser.options().len());
        });
    });
}

fn bench_option_def_validation(c: &mut Criterion) {
    let def = OptionDef::new("split", OptionType::Integer)
        .default(OptionValue::Int(1))
        .range(1, 16);
    let test_vals: Vec<String> = (0..100).map(|i| (i + 1).to_string()).collect();
    c.bench(BenchmarkId::new("option_validate_100_ints"), |b| {
        b.iter_with_large_input(&test_vals, |vals| {
            for v in vals.iter() {
                let r = def.parse_value(v);
                std::hint::black_box(r.is_ok());
            }
        });
    });
}

#[tokio::main(flavor = "current_thread")]
async fn bench_get_global_option(c: &mut Criterion) {
    let mgr = ConfigManager::new();
    mgr.set_global_option("split", OptionValue::Int(8)).await.unwrap();

    c.bench_function("get_global_option_i64", |b| {
        b.to_async(&tokio::runtime::Handle::current(), |async {
            let v = mgr.get_global_i64("split").await;
            black_box(v);
        });
    });

    c.bench_function("get_global_option_str", |b| {
        b.to_async(&tokio::runtime::Handle::current(), |async {
            let v = mgr.get_global_str("dir").await;
            black_box(v);
        });
    });
}

#[tokio::main(flavor = "current_thread")]
async fn bench_set_global_option(c: &mut Criterion) {
    c.bench_function("set_global_option_int", |b| {
        b.to_async(&tokio::runtime::Handle::current(), |async {
            let mut mgr = ConfigManager::new();
            for i in 0..100 {
                let _ = mgr.set_global_option("split", OptionValue::Int(i)).await;
            }
        });
    });
}

#[tokio::main(flavor = "current_thread")]
async fn bench_batch_set_options(c: &mut Criterion) {
    let opts: Vec<(String, String)> = (0..20)
        .map(|i| (format!("opt{}", i), format!("val{}", i)))
        .collect();
    c.bench(BenchmarkId::new("batch_set_20_options"), |b| {
        b.to_async(&tokio::runtime::Handle::current(), |async {
            let mut mgr = ConfigManager::new();
            let mut errors = Vec::new();
            for (k, v) in opts.iter() {
                if let Err(e) = mgr.set_global_option(k, OptionValue::Str(v.clone())).await {
                    errors.push(e);
                }
            }
            black_box(errors.len());
        });
    });
}

#[tokio::main(flavor = "current_thread")]
async fn bench_create_task_config(c: &mut Criterion) {
    let mut overrides = std::collections::HashMap::new();
    for i in 0..10 {
        overrides.insert(format!("custom_opt{}", i), OptionValue::Str(format!("val{}", i)));
    }

    c.bench(BenchmarkId::new("create_task_config_10_overrides"), |b| {
        b.to_async(&tokio::runtime::Handle::current(), |async {
            let mgr = ConfigManager::new();
            for _ in 0..100 {
                let config = mgr.create_task_config(overrides.clone()).await;
                black_box(config.len());
            }
        });
    });
}

#[tokio::main(flavor = "current_thread")]
async fn bench_change_event_broadcast(c: &mut Criterion) {
    c.bench(BenchmarkId::new("change_event_publish_subscribe"), |b| {
        b.to_async(&tokio::runtime::Handle::current(), |async {
            let mut mgr = ConfigManager::new();
            let rx = mgr.subscribe_changes();
            for i in 0..1000 {
                let _ = mgr.set_global_option("split", OptionValue::Int(i)).await;
            }
            drop(mgr);
            let mut count = 0;
            while let Ok(_) = rx.try_recv() { count += 1; if count >= 1000 { break; }
            black_box(count);
        });
    });
}

#[tokio::main(flavor = "current_thread")]
async fn bench_save_load_session(c: &mut Criterion) {
    let tmp_dir = tempfile::tempdir().unwrap();
    let path = tmp_dir.path().join("session.txt");
    let path_str = path.to_string_lossy().to_string();

    c.bench(BenchmarkId::new("session_save_load_roundtrip"), |b| {
        b.to_async(&tokio::runtime::Handle::current(), |async {
            let mut mgr1 = ConfigManager::new();
            for i in 0..50 {
                let _ = mgr1.set_global_option(&format!("opt{}", i), OptionValue::Int(i)).await;
            }
            mgr1.save_session(&path_str).await.unwrap();

            let mut mgr2 = ConfigManager::new();
            mgr2.load_session(&path_str).await.unwrap();
            black_box(mgr2.get_global_i64("opt0").await);
        });
    });
}

#[tokio::main(flavor = "current_thread")]
async fn bench_get_all_json(c: &mut Criterion) {
    let mgr = ConfigManager::new();
    for i in 0..50 {
        mgr.set_global_option(&format!("opt{}", i), OptionValue::Int(i)).await.unwrap();
    }

    c.bench_function("get_all_global_options_json", |b| {
        b.to_async(&tokio::runtime::Handle::current(), |async {
            let json = mgr.get_all_global_options_json().await;
            black_box(json.as_object().unwrap_or(&serde_json::Map::new()).len());
        });
    });
}

fn bench_parse_uri_list(c: &mut Criterion) {
    let content = gen_uri_list_content(100);
    c.bench(BenchmarkId::new("parse_uri_list_100_entries"), |b| {
        b.iter_with_large_input(&content, |content| {
            let mut file = UriListFile::new();
            file.parse(content).ok();
            black_box(file.len());
        });
    });
}

fn bench_parse_netrc(c: &mut Criterion) {
    let content = gen_netrc_content(50);
    c.bench(BenchmarkId::new("parse_netrc_50_entries"), |b| {
        b.iter_with_large_input(&content, |content| {
            let mut file = NetRcFile::new();
            file.parse(content).ok();
            black_box(file.len());
        });
    });
}

criterion_group!(config_benches,
    bench_registry_lookup,
    bench_registry_iterate,
    bench_registry_contains,
    bench_parse_size_str,
    bench_format_size,
    bench_parse_cli_args,
    bench_option_def_validation,
    bench_get_global_option,
    bench_set_global_option,
    bench_batch_set_options,
    bench_create_task_config,
    bench_change_event_broadcast,
    bench_save_load_session,
    bench_get_all_json,
    bench_parse_uri_list,
    bench_parse_netrc,
);

fn main() {
    let mut c = Criterion::default().sample_size(100).warm_up_time(std::time::Duration::from_millis(500));
    config_benches(&mut c);
}
