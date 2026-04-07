use criterion::{criterion_group, Criterion, black_box};
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::request::request_group::Gid;

fn bench_engine_creation(c: &mut Criterion) {
    c.bench_function("engine_create_destroy", |b| {
        b.iter(|| {
            let engine = DownloadEngine::new(100);
            black_box(engine.tick_interval_ms());
        });
    });
}

fn bench_gid_generation(c: &mut Criterion) {
    c.bench_function("gid_generation_rate", |b| {
        b.iter(|| {
            for _ in 0..100 {
                let gid = Gid::generate();
                black_box(gid.value());
            }
        });
    });
}

fn bench_add_single_group(c: &mut Criterion) {
    c.bench_function("add_group_single", |b| {
        b.iter(|| {
            let man = RequestGroupMan::new();
            let opts = DownloadOptions::default();
            let gid = man.add_group(vec!["http://example.com/file.zip".into()], opts);
            match gid {
                Ok(g) => black_box(g.value()),
                Err(e) => black_box(e.to_string()),
            }
        });
    });
}

fn bench_add_batch_groups(c: &mut Criterion) {
    c.bench(BenchmarkId::new("add_group_batch_100"), |b| {
        b.iter(|| {
            let man = RequestGroupMan::new();
            let opts = DownloadOptions::default();
            let mut total = 0u64;
            for i in 0..100u32 {
                let uri = format!("http://example.com/file{}.zip", i);
                match man.add_group(vec![uri], opts.clone()) {
                    Ok(g) => total += g.value(),
                    Err(_) => {}
                }
            }
            black_box(total);
        });
    });
}

fn bench_segment_bitfield_ops(c: &mut Criterion) {
    use aria2_core::segment::bitfield::Bitfield;
    c.bench(BenchmarkId::new("bitfield_set_test_10000_ops"), |b| {
        b.iter(|| {
            let bf = Bitfield::new(100000);
            for i in 0..10000usize {
                bf.set(i);
                let v = bf.test(i);
                std::hint::black_box(v);
            }
        });
    });
}

fn bench_segment_split(c: &mut Criterion) {
    use aria2_core::segment::Segment;
    c.bench(BenchmarkId::new("segment_split_1MB_into_16_parts"), |b| {
        b.iter(|| {
            let segments: Vec<Segment> = Segment::split_file(1024 * 1024, 16);
            black_box(segments.len());
        });
    });
}

fn bench_option_value_display(c: &mut Criterion) {
    use aria2_core::config::OptionValue;
    let values: Vec<OptionValue> = vec![
        OptionValue::Str("a long string value with some data".into()),
        OptionValue::Int(999999999),
        OptionValue::Float(3.14159265358979),
        OptionValue::Bool(true),
        OptionValue::List(vec!["x".into(), "y".into(), "z".into()]),
    ];
    c.bench(BenchmarkId::new("optionvalue_display_5_types"), |b| {
        b.iter_with_black_input(&values, |vals| {
            for v in vals.iter() {
                let s = format!("{}", v);
                std::hint::black_box(s);
            }
        });
    });
}

fn bench_format_speed_duration(c: &mut Criterion) {
    let speeds: Vec<u64> = (0..100).map(|i| (i + 1) * 1024 * 100).collect();
    let durations: Vec<f64> = (0..100).map(|i| (i as f64) * 1.5).collect();

    c.bench(BenchmarkId::new("format_speed_100_values"), |b| {
        b.iter_with_black_input(&speeds, |spds| {
            for s in spds.iter() {
                let f = aria2_core::ui::format_speed(*s);
                std::hint::black_box(f);
            }
        });
    });

    c.bench(BenchmarkId::new("format_duration_100_values"), |b| {
        b.iter_with_black_input(&durations, |durs| {
            for d in dur.iter() {
                let f = aria2_core::ui::format_duration(*d);
                std::hint::black_box(f);
            }
        });
    });
}

fn bench_progress_bar_render(c: &mut Criterion) {
    use aria2_core::ui::ProgressBar;
    c.bench(BenchmarkId::new("progress_bar_render_100_updates"), |b| {
        b.iter(|| {
            let mut pb = ProgressBar::new(1024 * 1024 * 100);
            for i in 0..100 {
                pb.update((i + 1) * 1024 * 1024);
                pb.render(true);
            }
            pb.finish();
        });
    });
}

fn bench_multi_progress_render(c: &mut Criterion) {
    use aria2_core::ui::MultiProgress;
    c.bench(BenchmarkId::new("multi_progress_10_tasks_50_updates"), |b| {
        b.iter(|| {
            let mut mp = MultiProgress::new();
            for i in 0..10 {
                mp.add(&format!("task{}", i), 1024 * 1024 * 10);
            }
            for step in 0..50 {
                for i in 0..10 {
                    mp.update(i, (step + 1) * 1024 * 1024 / 50);
                }
            }
            mp.finish_all();
        });
    });
}

criterion_group!(engine_benches,
    bench_engine_creation,
    bench_gid_generation,
    bench_add_single_group,
    bench_add_batch_groups,
    bench_segment_bitfield_ops,
    bench_segment_split,
    bench_option_value_display,
    bench_format_speed_duration,
    bench_progress_bar_render,
    bench_multi_progress_render,
);

fn main() {
    let mut c = Criterion::default().sample_size(100).warm_up_time(std::time::Duration::from_millis(300));
    engine_benches(&mut c);
}
