// Performance benchmark for sequential piece selection optimization
// This benchmark demonstrates the O(1) complexity improvement

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use aria2_protocol::bittorrent::piece::picker::{PiecePicker, PieceSelectionStrategy, PiecePriorityMode};

fn bench_sequential_selection(c: &mut Criterion) {
    let mut group = c.benchmark_group("sequential_selection");
    
    // Test different sizes to show O(1) complexity
    for size in [100, 1000, 10000, 50000].iter() {
        // Benchmark: Sequential strategy with cursor optimization
        group.bench_with_input(BenchmarkId::new("optimized_sequential", size), size, |b, &size| {
            b.iter(|| {
                let mut picker = PiecePicker::new(size);
                picker.set_strategy(PieceSelectionStrategy::Sequential);
                
                // Mark half of the pieces as completed
                for i in 0..(size / 2) {
                    picker.mark_completed(i);
                }
                
                // Pick next piece (should be O(1) with cursor)
                black_box(picker.pick_next())
            });
        });
        
        // Benchmark: SequentialHead mode with cursor optimization
        group.bench_with_input(BenchmarkId::new("optimized_head", size), size, |b, &size| {
            b.iter(|| {
                let mut picker = PiecePicker::new(size);
                picker.set_priority_mode(PiecePriorityMode::SequentialHead);
                
                for i in 0..(size / 2) {
                    picker.mark_completed(i);
                }
                
                black_box(picker.pick_next())
            });
        });
        
        // Benchmark: SequentialTail mode with cursor optimization
        group.bench_with_input(BenchmarkId::new("optimized_tail", size), size, |b, &size| {
            b.iter(|| {
                let mut picker = PiecePicker::new(size);
                picker.set_priority_mode(PiecePriorityMode::SequentialTail);
                
                for i in (size / 2)..size {
                    picker.mark_completed(i);
                }
                
                black_box(picker.pick_next())
            });
        });
    }
    
    group.finish();
}

fn bench_cursor_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("cursor_update");
    
    // Benchmark cursor update performance when marking pieces as completed
    group.bench_function("mark_completed_updates_cursor", |b| {
        let mut picker = PiecePicker::new(10000);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        
        b.iter(|| {
            // Mark a piece as completed and update cursor
            for i in 0..100 {
                picker.mark_completed(i);
            }
            black_box(picker.pick_next())
        });
    });
    
    // Benchmark cursor update with in_progress changes
    group.bench_function("mark_in_progress_updates_cursor", |b| {
        let mut picker = PiecePicker::new(10000);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        
        b.iter(|| {
            for i in 0..100 {
                picker.mark_in_progress(i, true);
            }
            black_box(picker.pick_next())
        });
    });
    
    group.finish();
}

fn bench_real_world_scenario(c: &mut Criterion) {
    let mut group = c.benchmark_group("real_world");
    
    // Simulate real-world torrent download scenario
    group.bench_function("sequential_download_simulation", |b| {
        b.iter(|| {
            let mut picker = PiecePicker::new(10000);
            picker.set_strategy(PieceSelectionStrategy::Sequential);
            
            // Simulate downloading pieces one by one
            let mut downloaded = 0;
            while let Some(piece) = picker.pick_next() {
                picker.mark_completed(piece);
                downloaded += 1;
                if downloaded >= 1000 {
                    break;
                }
            }
            
            black_box(downloaded)
        });
    });
    
    // Simulate streaming scenario (SequentialHead)
    group.bench_function("streaming_download_simulation", |b| {
        b.iter(|| {
            let mut picker = PiecePicker::new(10000);
            picker.set_priority_mode(PiecePriorityMode::SequentialHead);
            
            let mut downloaded = 0;
            while let Some(piece) = picker.pick_next() {
                picker.mark_completed(piece);
                downloaded += 1;
                if downloaded >= 1000 {
                    break;
                }
            }
            
            black_box(downloaded)
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_sequential_selection,
    bench_cursor_update,
    bench_real_world_scenario
);
criterion_main!(benches);
