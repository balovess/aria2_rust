use aria2_core::engine::bt_message_handler::{BLOCK_SIZE, BtPeerMessageHandler};
use aria2_core::engine::bt_peer_connection::PeerSessionResource;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group::GroupId;
use aria2_core::request::request_group::{DownloadOptions, RequestGroup};
use aria2_core::segment::PieceStatMan;
use aria2_core::segment::Segment;
use aria2_core::segment::bitfield::Bitfield;
use aria2_core::ui::{MultiProgress, ProgressBar};
use aria2_protocol::bittorrent::message::serializer::serialize;
use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group};
use std::time::Duration;

fn bench_engine_creation(c: &mut Criterion) {
    c.bench_function("engine_create_destroy", |b| {
        b.iter(|| {
            let engine = DownloadEngine::new();
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
                let _ = bf.clear(i);
            }
            black_box(bf.len());
        });
    });
}

fn bench_bt_piece_completion_bitfield(c: &mut Criterion) {
    let mut group = c.benchmark_group("bt_piece_completion_bitfield");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for piece_count in [100_000usize, 1_000_000] {
        let byte_len = piece_count.div_ceil(8);
        let piece_index = piece_count - 1;

        group.bench_with_input(
            BenchmarkId::new("legacy_full_snapshot", piece_count),
            &piece_count,
            |b, _| {
                b.iter_batched(
                    || vec![0u8; byte_len],
                    |mut bitfield| {
                        bitfield[piece_index / 8] |= 1 << (7 - (piece_index % 8));
                        // Previous path: PiecePicker export plus RequestGroup clone.
                        let exported = bitfield.clone();
                        let group_copy = exported.clone();
                        black_box((exported, group_copy));
                    },
                    BatchSize::SmallInput,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("shared_single_bit_update", piece_count),
            &piece_count,
            |b, _| {
                b.iter_batched(
                    || {
                        let request_group = RequestGroup::new(
                            GroupId::new(0),
                            vec!["magnet:?xt=urn:btih:benchmark".to_string()],
                            DownloadOptions::default(),
                        );
                        request_group.set_bt_bitfield(Some(vec![0u8; byte_len]));
                        request_group
                    },
                    |request_group| {
                        request_group
                            .update_bt_bitfield_piece(piece_index as u32, piece_count as u32);
                        black_box(request_group);
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

fn bench_peer_have_transition(c: &mut Criterion) {
    let mut group = c.benchmark_group("bt_peer_have_transition");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for piece_count in [100_000usize, 1_000_000] {
        let piece_index = piece_count - 1;

        group.bench_with_input(
            BenchmarkId::new("legacy_full_transition", piece_count),
            &piece_count,
            |b, _| {
                b.iter_batched(
                    || {
                        (
                            PeerSessionResource::new(1, piece_count as u64),
                            PieceStatMan::new(piece_count, false),
                        )
                    },
                    |(mut peer, stats)| {
                        let old = peer.bitfield().to_vec();
                        peer.update_bitfield(piece_index, 1);
                        let new = peer.bitfield().to_vec();
                        let is_seeder = peer.is_seeder();
                        stats.update_piece_stats(&new, &old);
                        black_box((is_seeder, stats.counts_ref()[piece_index]));
                    },
                    BatchSize::SmallInput,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("incremental_transition", piece_count),
            &piece_count,
            |b, _| {
                b.iter_batched(
                    || {
                        (
                            PeerSessionResource::new(1, piece_count as u64),
                            PieceStatMan::new(piece_count, false),
                        )
                    },
                    |(mut peer, stats)| {
                        peer.update_bitfield(piece_index, 1);
                        let is_seeder = peer.is_seeder();
                        stats.update_piece_stat(piece_index, false, true);
                        black_box((is_seeder, stats.counts_ref()[piece_index]));
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

fn bench_bt_request_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("bt_request_queue");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for request_count in [1_000usize, 10_000, 100_000] {
        let requests: Vec<(u32, u32)> = (0..request_count)
            .map(|index| (index as u32, (index as u32) * BLOCK_SIZE))
            .collect();

        group.bench_with_input(
            BenchmarkId::new("single_serialization_and_enqueue", request_count),
            &requests,
            |b, requests| {
                b.iter_batched(
                    || BtPeerMessageHandler::with_max_outstanding(BLOCK_SIZE, request_count),
                    |mut handler| {
                        for &(index, begin) in requests {
                            let message = BtMessage::Request {
                                request: PieceBlockRequest::new(index, begin, BLOCK_SIZE),
                            };
                            let serialized = serialize(&message);
                            assert!(handler.send_request(index, begin, BLOCK_SIZE, serialized));
                        }
                        black_box(handler);
                    },
                    BatchSize::SmallInput,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("double_serialization_and_enqueue", request_count),
            &requests,
            |b, requests| {
                b.iter_batched(
                    || BtPeerMessageHandler::with_max_outstanding(BLOCK_SIZE, request_count),
                    |mut handler| {
                        for &(index, begin) in requests {
                            let message = BtMessage::Request {
                                request: PieceBlockRequest::new(index, begin, BLOCK_SIZE),
                            };
                            let serialized = serialize(&message);
                            assert!(handler.send_request(index, begin, BLOCK_SIZE, serialized));
                            // Previous send_request implementations serialized the same
                            // request again after it had already been queued.
                            black_box(serialize(&message));
                        }
                        black_box(handler);
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
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
                mp.add(format!("task{}", i), 1024 * 1024 * 10);
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
    bench_bt_piece_completion_bitfield,
    bench_peer_have_transition,
    bench_bt_request_queue,
    bench_segment_creation,
    bench_progress_bar_render,
    bench_multi_progress_render,
);

fn main() {
    engine_benches();
}
