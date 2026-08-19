//! Benchmark bitfield-wide piece-stat updates and stream-selection queries.

use aria2_core::segment::{BitfieldMan, PieceStatMan};
use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

fn make_pattern(num_pieces: usize, stride: usize, phase: usize) -> Vec<u8> {
    let mut bitfield = vec![0u8; num_pieces.div_ceil(8)];
    for index in (phase..num_pieces).step_by(stride) {
        bitfield[index / 8] |= 1 << (7 - index % 8);
    }
    bitfield
}

fn make_bitfield_man(num_pieces: usize) -> BitfieldMan {
    let mut manager = BitfieldMan::new(1024, num_pieces as u64 * 1024);
    let completed = make_pattern(num_pieces, 4, 0);
    manager.set_bitfield(&completed);
    for index in (1..num_pieces).step_by(17) {
        manager.set_use_piece(index);
    }
    manager
}

fn bench_piece_stat_updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("piece_stat_bitfield_updates");

    for size in [1_000usize, 10_000, 50_000] {
        let sparse = make_pattern(size, 17, 0);
        let dense = make_pattern(size, 2, 0);
        let old = make_pattern(size, 4, 0);
        let new = make_pattern(size, 4, 1);

        for (name, bitfield) in [("sparse", &sparse), ("dense", &dense)] {
            group.bench_with_input(
                BenchmarkId::new(format!("add/{name}"), size),
                bitfield,
                |b, bitfield| {
                    b.iter_batched(
                        || PieceStatMan::new(size, false),
                        |manager| {
                            manager.add_piece_stats_bitfield(bitfield);
                            black_box(manager.counts_ref()[size / 2]);
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }

        group.bench_with_input(
            BenchmarkId::new("update/changed_bits", size),
            &(&new, &old),
            |b, (new, old)| {
                b.iter_batched(
                    || PieceStatMan::new(size, false),
                    |manager| {
                        manager.update_piece_stats(new, old);
                        black_box(manager.counts_ref()[size / 2]);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_bitfield_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("bitfield_piece_queries");

    for size in [1_000usize, 10_000, 50_000] {
        let manager = make_bitfield_man(size);
        let peer = make_pattern(size, 3, 0);
        let ignore = make_pattern(size, 13, 0);

        group.bench_with_input(
            BenchmarkId::new("all_missing_indexes", size),
            &manager,
            |b, manager| {
                b.iter(|| black_box(manager.all_missing_indexes(&peer)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("all_missing_unused_indexes", size),
            &manager,
            |b, manager| {
                b.iter(|| black_box(manager.all_missing_unused_indexes(&peer)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("sparse_missing_unused_index", size),
            &manager,
            |b, manager| {
                b.iter(|| black_box(manager.get_sparse_missing_unused_index(1024 * 8, &ignore)));
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_piece_stat_updates, bench_bitfield_queries);
criterion_main!(benches);
