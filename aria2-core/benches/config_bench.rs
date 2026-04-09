use aria2_core::config::{OptionDef, OptionRegistry, OptionType, OptionValue};
use criterion::{black_box, criterion_group, BenchmarkId, Criterion};

fn gen_registry() -> OptionRegistry {
    OptionRegistry::new()
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
        b.iter(|| black_box(reg.all().values().count()));
    });
}

fn bench_registry_contains(c: &mut Criterion) {
    let reg = gen_registry();
    c.bench_function("registry_contains", |b| {
        b.iter(|| {
            let r1 = reg.contains("split");
            let r2 = reg.contains("nonexistent");
            black_box(r1 || r2);
        });
    });
}

fn bench_option_def_validation(c: &mut Criterion) {
    let def = OptionDef::new("split", OptionType::Integer)
        .default(OptionValue::Int(1))
        .range(1, 16);
    let test_vals: Vec<String> = (0..100).map(|i| (i + 1).to_string()).collect();
    c.bench_with_input(
        BenchmarkId::new("option_validate_100_ints", 100),
        &test_vals,
        |b, vals| {
            b.iter(|| {
                for v in vals.iter() {
                    let r = def.parse_value(v);
                    black_box(r.is_ok());
                }
            });
        },
    );
}

fn bench_option_value_display(c: &mut Criterion) {
    let values: Vec<OptionValue> = vec![
        OptionValue::Str("a long string value".into()),
        OptionValue::Int(999999999),
        OptionValue::Float(3.14159265358979),
        OptionValue::Bool(true),
        OptionValue::List(vec!["x".into(), "y".into(), "z".into()]),
    ];
    c.bench_with_input(
        BenchmarkId::new("optionvalue_display_5_types", 5),
        &values,
        |b, vals| {
            b.iter(|| {
                for v in vals.iter() {
                    let s = format!("{}", v);
                    black_box(s);
                }
            });
        },
    );
}

fn bench_format_size_inline(c: &mut Criterion) {
    let values: Vec<u64> = (0..100).map(|i| (i + 1) * 1024).collect();
    c.bench_with_input(
        BenchmarkId::new("format_size_inline_100", 100),
        &values,
        |b, vals| {
            b.iter(|| {
                for v in vals.iter() {
                    let s = if *v < 1024 {
                        format!("{}B", v)
                    } else if *v < 1024 * 1024 {
                        format!("{:.1}KiB", *v as f64 / 1024.0)
                    } else if *v < 1024 * 1024 * 1024 {
                        format!("{:.1}MiB", *v as f64 / (1024.0 * 1024.0))
                    } else {
                        format!("{:.1}GiB", *v as f64 / (1024.0 * 1024.0 * 1024.0))
                    };
                    black_box(s);
                }
            });
        },
    );
}

fn bench_format_speed_inline(c: &mut Criterion) {
    let speeds: Vec<u64> = (0..100).map(|i| (i + 1) * 1024 * 100).collect();
    c.bench_with_input(
        BenchmarkId::new("format_speed_inline_100", 100),
        &speeds,
        |b, spds| {
            b.iter(|| {
                for s in spds.iter() {
                    let f = if *s < 1024 {
                        format!("{}B/s", s)
                    } else if *s < 1024 * 1024 {
                        format!("{:.1}KiB/s", *s as f64 / 1024.0)
                    } else if *s < 1024 * 1024 * 1024 {
                        format!("{:.1}MiB/s", *s as f64 / (1024.0 * 1024.0))
                    } else {
                        format!("{:.1}GiB/s", *s as f64 / (1024.0 * 1024.0 * 1024.0))
                    };
                    black_box(f);
                }
            });
        },
    );
}

fn bench_parse_cli_args(c: &mut Criterion) {
    use aria2_core::config::parser::ConfigParser;
    let args: Vec<&str> = vec![
        "--dir=/tmp",
        "--split=8",
        "--max-tries=5",
        "--timeout=60",
        "--quiet=true",
        "--max-download-limit=0",
        "--check-certificate=true",
    ];
    let registry = gen_registry();
    c.bench_with_input(
        BenchmarkId::new("parse_cli_args_7_args", 7),
        &args,
        |b, args| {
            b.iter(|| {
                let mut parser = ConfigParser::with_registry(registry.clone());
                parser.parse_cli_args(args);
                black_box(parser.options().len());
            });
        },
    );
}

criterion_group!(
    config_benches,
    bench_registry_lookup,
    bench_registry_iterate,
    bench_registry_contains,
    bench_option_def_validation,
    bench_option_value_display,
    bench_format_size_inline,
    bench_format_speed_inline,
    bench_parse_cli_args,
);

fn main() {
    config_benches();
}
