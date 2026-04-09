use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group::GroupId;
use aria2_core::segment::bitfield::Bitfield;
use aria2_core::segment::Segment;
use aria2_core::ui::{MultiProgress, ProgressBar};
use criterion::{black_box, criterion_group, Criterion};

fn bench_engine_creation(c: &mut Criterion) {
    c.bench_function("engine_create_destroy", |b| {
        b.iter(|| {
            let engine = DownloadEngine::new(100);
            black_box(std::mem::size_of_val(&engine));
        });
    });
}

fn bench_group_id_generation(c: &mut Criterion) {
    c.bench_function("group_id_generation", |b| {
        b.iter(|| {
            for i in 0..100u64 {
                let gid = GroupId::new(i);
                black_box(gid.value());
            }
        });
    });
}

fn bench_bitfield_set_unset(c: &mut Criterion) {
    c.bench_function("bitfield_set_unset_10000_ops", |b| {
        b.iter(|| {
            let mut bf = Bitfield::new(100000);
            for i in 0..10000usize {
                let _ = bf.set(i);
                let _ = bf.unset(i);
            }
            black_box(bf.len());
        });
    });
}

fn bench_segment_creation(c: &mut Criterion) {
    c.bench_function("segment_creation_16", |b| {
        b.iter(|| {
            let segment_size = 1024 * 1024 / 16;
            let segments: Vec<Segment> = (0..16)
                .map(|i| {
                    Segment::new(
                        i,
                        (i as u64) * segment_size,
                        ((i + 1) as u64) * segment_size,
                    )
                })
                .collect();
            black_box(segments.len());
        });
    });
}

fn bench_progress_bar_render(c: &mut Criterion) {
    c.bench_function("progress_bar_render_100_updates", |b| {
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
    c.bench_function("multi_progress_10_tasks_50_updates", |b| {
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

criterion_group!(
    engine_benches,
    bench_engine_creation,
    bench_group_id_generation,
    bench_bitfield_set_unset,
    bench_segment_creation,
    bench_progress_bar_render,
    bench_multi_progress_render,
);

fn main() {
    engine_benches();
}
