//! Rust-only high-concurrency engine-loop benchmark.
//!
//! This is an explicit benchmark test rather than a CI assertion because its
//! CPU and RSS numbers depend on the host. Run it with `--ignored --nocapture`
//! to record a repeatable local baseline for the current Rust implementation.

mod e2e_helpers;

use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use e2e_helpers::mock_http_server::MockHttpServer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};
use tempfile::TempDir;

const FILE_SIZE: usize = 512 * 1024;
const SPLIT_COUNT: u16 = 4;

fn benchmark_parameter(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value: &usize| value > 0)
        .unwrap_or(default)
}

#[derive(Clone, Copy, Default)]
struct ProcessSamples {
    rss_before: u64,
    peak_rss: u64,
    cpu_sum_milli_percent: u64,
    cpu_max_milli_percent: u64,
    cpu_samples: u64,
}

#[derive(Clone, Copy, Default)]
struct LockWaitSamples {
    samples: u64,
    total_us: u64,
    max_us: u64,
}

fn sample_process(sys: &mut System, pid: Pid, samples: &mut ProcessSamples) {
    if !sys.refresh_process(pid) {
        return;
    }
    let Some(process) = sys.process(pid) else {
        return;
    };
    let rss = process.memory();
    samples.peak_rss = samples.peak_rss.max(rss);
    let cpu_milli_percent = (process.cpu_usage() * 1000.0) as u64;
    samples.cpu_sum_milli_percent = samples
        .cpu_sum_milli_percent
        .saturating_add(cpu_milli_percent);
    samples.cpu_max_milli_percent = samples.cpu_max_milli_percent.max(cpu_milli_percent);
    samples.cpu_samples += 1;
}

async fn sample_process_until_stopped(stop: Arc<AtomicBool>, samples: Arc<Mutex<ProcessSamples>>) {
    let pid = Pid::from_u32(std::process::id());
    let mut sys = System::new();
    while !stop.load(Ordering::Relaxed) {
        if let Ok(mut samples) = samples.lock() {
            sample_process(&mut sys, pid, &mut samples);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if let Ok(mut samples) = samples.lock() {
        sample_process(&mut sys, pid, &mut samples);
    }
}

async fn sample_group_lock_wait_until_stopped(
    stop: Arc<AtomicBool>,
    groups: Vec<Arc<std::sync::RwLock<RequestGroup>>>,
    samples: Arc<Mutex<LockWaitSamples>>,
) {
    while !stop.load(Ordering::Relaxed) {
        for group in &groups {
            let mut first_failure: Option<Instant> = None;
            loop {
                if group.try_read().is_ok() {
                    if let Some(start) = first_failure {
                        let wait_us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
                        if let Ok(mut samples) = samples.lock() {
                            samples.samples += 1;
                            samples.total_us = samples.total_us.saturating_add(wait_us);
                            samples.max_us = samples.max_us.max(wait_us);
                        }
                    }
                    break;
                }
                first_failure.get_or_insert_with(Instant::now);
                tokio::task::yield_now().await;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn benchmark_data() -> Vec<u8> {
    (0..FILE_SIZE)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(7))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "host-dependent performance baseline; run explicitly with --ignored --nocapture"]
async fn rust_engine_loop_high_concurrency_baseline() {
    let download_count = benchmark_parameter("ARIA2_BENCH_DOWNLOADS", 48);
    let max_concurrent = benchmark_parameter("ARIA2_BENCH_MAX_CONCURRENT", 16) as u32;
    let server = MockHttpServer::start()
        .await
        .expect("failed to start local benchmark HTTP server");
    let data = benchmark_data();
    server.register_range_response("/engine-loop/", &data);

    let temp_dir = TempDir::new().expect("failed to create benchmark output directory");
    let group_man = Arc::new(RequestGroupMan::new());
    group_man.set_max_concurrent(max_concurrent);

    let mut engine = DownloadEngine::new(1);
    engine.set_request_group_man(Arc::clone(&group_man));
    let command_tx = engine.engine_command_sender();

    let mut groups = Vec::with_capacity(download_count);
    for index in 0..download_count {
        let options = DownloadOptions {
            split: Some(SPLIT_COUNT),
            max_connection_per_server: Some(SPLIT_COUNT),
            use_head: true,
            dir: Some(temp_dir.path().to_string_lossy().into_owned()),
            out: Some(format!("engine-loop-{index}.bin")),
            ..DownloadOptions::default()
        };
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(index as u64 + 1),
            vec![format!("{}/engine-loop/{index}.bin", server.base_url())],
            options,
        )));
        groups.push(Arc::clone(&group));
        command_tx
            .send(EngineCommand::AddDownload { group })
            .expect("benchmark command queue should accept initial workload");
    }

    let process_samples = Arc::new(Mutex::new(ProcessSamples::default()));
    let mut process_sys = System::new();
    let process_pid = Pid::from_u32(std::process::id());
    if process_sys.refresh_process(process_pid)
        && let Some(process) = process_sys.process(process_pid)
        && let Ok(mut samples) = process_samples.lock()
    {
        samples.rss_before = process.memory();
        samples.peak_rss = samples.rss_before;
    }
    let process_stop = Arc::new(AtomicBool::new(false));
    let process_sampler = tokio::spawn(sample_process_until_stopped(
        Arc::clone(&process_stop),
        Arc::clone(&process_samples),
    ));

    let lock_samples = Arc::new(Mutex::new(LockWaitSamples::default()));
    let lock_stop = Arc::new(AtomicBool::new(false));
    let lock_sampler = tokio::spawn(sample_group_lock_wait_until_stopped(
        Arc::clone(&lock_stop),
        groups.clone(),
        Arc::clone(&lock_samples),
    ));

    let total_bytes = (download_count * FILE_SIZE) as u64;
    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(60), engine.run()).await;
    let elapsed = start.elapsed();

    process_stop.store(true, Ordering::Relaxed);
    lock_stop.store(true, Ordering::Relaxed);
    process_sampler
        .await
        .expect("process sampler should not panic");
    lock_sampler.await.expect("lock sampler should not panic");

    assert!(
        result.is_ok(),
        "engine benchmark timed out: {:?}",
        result.err()
    );
    assert!(
        result.unwrap().is_ok(),
        "engine benchmark returned an error"
    );

    for index in 0..download_count {
        let path = temp_dir.path().join(format!("engine-loop-{index}.bin"));
        let output = tokio::fs::read(&path)
            .await
            .unwrap_or_else(|error| panic!("missing benchmark output {}: {error}", path.display()));
        assert_eq!(
            output.len(),
            data.len(),
            "benchmark output {} has the wrong length",
            path.display()
        );
        assert_eq!(
            &output[..64],
            &data[..64],
            "benchmark output {} has corrupted leading bytes",
            path.display()
        );
        assert_eq!(
            &output[output.len() - 64..],
            &data[data.len() - 64..],
            "benchmark output {} has corrupted trailing bytes",
            path.display()
        );
    }

    let queue = command_tx.snapshot();
    let process = *process_samples.lock().unwrap();
    let lock = *lock_samples.lock().unwrap();
    let throughput_mib_s =
        total_bytes as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE) / (1024.0 * 1024.0);
    let avg_cpu_percent = if process.cpu_samples == 0 {
        0.0
    } else {
        process.cpu_sum_milli_percent as f64 / process.cpu_samples as f64 / 1000.0
    };
    let max_cpu_percent = process.cpu_max_milli_percent as f64 / 1000.0;
    let avg_dispatch_us = queue
        .dispatch_latency_us_total
        .checked_div(queue.dispatch_samples)
        .unwrap_or(0);
    let avg_lock_wait_us = lock.total_us.checked_div(lock.samples).unwrap_or(0);

    println!(
        "RUST_ENGINE_BASELINE {{\"downloads\":{download_count},\"bytes\":{total_bytes},\
\"elapsed_ms\":{},\"throughput_mib_s\":{throughput_mib_s:.3},\
\"cpu_avg_percent\":{avg_cpu_percent:.3},\"cpu_max_percent\":{max_cpu_percent:.3},\
\"rss_before_bytes\":{},\"rss_peak_bytes\":{},\"rss_delta_bytes\":{},\
\"queue_depth\":{},\"queue_max_depth\":{},\"queue_wakeups\":{},\
\"dispatch_samples\":{},\"dispatch_avg_us\":{avg_dispatch_us},\"dispatch_max_us\":{},\
\"lock_wait_samples\":{},\"lock_wait_avg_us\":{avg_lock_wait_us},\"lock_wait_max_us\":{} }}",
        elapsed.as_millis(),
        process.rss_before,
        process.peak_rss,
        process.peak_rss.saturating_sub(process.rss_before),
        queue.depth,
        queue.max_depth,
        queue.wakeups,
        queue.dispatch_samples,
        queue.dispatch_latency_us_max,
        lock.samples,
        lock.max_us,
    );

    assert_eq!(queue.depth, 0, "engine command queue must drain completely");
    assert!(
        queue.max_depth <= aria2_core::engine::engine_command::ENGINE_TOTAL_COMMAND_CAPACITY,
        "engine command queue depth exceeded its bound: {}",
        queue.max_depth
    );
    assert_eq!(queue.wakeups, 1);
    assert_eq!(queue.dispatch_samples, download_count as u64);

    server.shutdown().await;
}
