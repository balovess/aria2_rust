use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use aria2_protocol::bittorrent::piece::bitfield::Bitfield;

fn bench_bitfield_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("bitfield_vs_vec_bool");
    
    // Test different sizes
    for size in [100, 1_000, 10_000, 100_000].iter() {
        // Bitfield set operations
        group.bench_with_input(BenchmarkId::new("bitfield_set", size), size, |b, &size| {
            b.iter(|| {
                let mut bf = Bitfield::new(size);
                for i in 0..size {
                    bf.set(i).unwrap();
                }
                black_box(bf)
            })
        });
        
        // Vec<bool> set operations
        group.bench_with_input(BenchmarkId::new("vec_bool_set", size), size, |b, &size| {
            b.iter(|| {
                let mut vec = vec![false; size];
                for item in vec.iter_mut().take(size) {
                    *item = true;
                }
                black_box(vec)
            })
        });
        
        // Bitfield test operations
        let bf: Bitfield = {
            let mut bf = Bitfield::new(*size);
            for i in (0..*size).step_by(2) {
                bf.set(i).unwrap();
            }
            bf
        };
        
        let vec_bool: Vec<bool> = {
            let mut vec = vec![false; *size];
            for i in (0..*size).step_by(2) {
                vec[i] = true;
            }
            vec
        };
        
        group.bench_with_input(BenchmarkId::new("bitfield_test", size), size, |b, &size| {
            b.iter(|| {
                let mut count = 0;
                for i in 0..size {
                    if bf.test(i) {
                        count += 1;
                    }
                }
                black_box(count)
            })
        });
        
        group.bench_with_input(BenchmarkId::new("vec_bool_test", size), size, |b, &size| {
            b.iter(|| {
                let mut count = 0;
                for item in vec_bool.iter().take(size) {
                    if *item {
                        count += 1;
                    }
                }
                black_box(count)
            })
        });
        
        // Bitfield count operations
        group.bench_with_input(BenchmarkId::new("bitfield_count", size), size, |b, _| {
            b.iter(|| black_box(bf.count_set()))
        });
        
        // Vec<bool> count operations
        group.bench_with_input(BenchmarkId::new("vec_bool_count", size), size, |b, _| {
            b.iter(|| black_box(vec_bool.iter().filter(|&&x| x).count()))
        });
    }
    
    group.finish();
}

fn bench_memory_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_comparison");
    
    for size in [1_000, 10_000, 100_000, 1_000_000].iter() {
        // Bitfield memory
        let bf = Bitfield::new(*size);
        let bf_memory = bf.memory_usage();
        let vec_memory = bf.vec_bool_memory_usage();
        let ratio = bf.memory_savings_ratio();
        
        println!(
            "Size: {}, Bitfield: {} bytes, Vec<bool>: {} bytes, Ratio: {:.2}x",
            size, bf_memory, vec_memory, ratio
        );
        
        group.bench_with_input(BenchmarkId::new("bitfield_memory", size), size, |b, _| {
            b.iter(|| black_box(Bitfield::new(*size)))
        });
        
        group.bench_with_input(BenchmarkId::new("vec_bool_memory", size), size, |b, _| {
            b.iter(|| black_box(vec![false; *size]))
        });
    }
    
    group.finish();
}

fn bench_iteration(c: &mut Criterion) {
    let mut group = c.benchmark_group("iteration");
    
    let size = 10_000;
    let bf: Bitfield = {
        let mut bf = Bitfield::new(size);
        for i in (0..size).step_by(3) {
            bf.set(i).unwrap();
        }
        bf
    };
    
    let vec_bool: Vec<bool> = {
        let mut vec = vec![false; size];
        for i in (0..size).step_by(3) {
            vec[i] = true;
        }
        vec
    };
    
    group.bench_function("bitfield_iter_set", |b| {
        b.iter(|| {
            let count = bf.iter_set().count();
            black_box(count)
        })
    });
    
    group.bench_function("vec_bool_iter_true", |b| {
        b.iter(|| {
            let count = vec_bool.iter().enumerate().filter(|&(_, &x)| x).count();
            black_box(count)
        })
    });
    
    group.bench_function("bitfield_iter_clear", |b| {
        b.iter(|| {
            let count = bf.iter_clear().count();
            black_box(count)
        })
    });
    
    group.bench_function("vec_bool_iter_false", |b| {
        b.iter(|| {
            let count = vec_bool.iter().enumerate().filter(|&(_, &x)| !x).count();
            black_box(count)
        })
    });
    
    group.finish();
}

criterion_group!(benches, bench_bitfield_operations, bench_memory_usage, bench_iteration);
criterion_main!(benches);
