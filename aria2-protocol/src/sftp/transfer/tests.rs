use super::*;

#[cfg(feature = "sftp")]
mod integration;

#[test]
fn test_transfer_options_defaults() {
    let opts = TransferOptions::default();
    assert_eq!(opts.buffer_size, TRANSFER_BUF_SIZE);
    assert_eq!(opts.resume_offset, 0);
    assert!(!opts.preserve_permissions);
    assert!(!opts.preserve_time);
    assert!(opts.progress_callback.is_none());
}

#[test]
fn test_transfer_options_builder() {
    let opts = TransferOptions::default()
        .with_resume(4096)
        .with_buffer_size(128 * 1024)
        .preserve_metadata();

    assert_eq!(opts.resume_offset, 4096);
    assert_eq!(opts.buffer_size, 131072); // 128KB
    assert!(opts.preserve_permissions);
    assert!(opts.preserve_time);
}

#[test]
fn test_transfer_options_buffer_clamp() {
    let small = TransferOptions::default().with_buffer_size(512); // Below min
    assert_eq!(small.buffer_size, MIN_BUFFER_SIZE);

    let large = TransferOptions::default().with_buffer_size(2048 * 1024); // Above max
    assert_eq!(large.buffer_size, MAX_BUFFER_SIZE);

    let exact = TransferOptions::default().with_buffer_size(32768); // Valid
    assert_eq!(exact.buffer_size, 32768);
}

#[test]
fn transfer_uses_a_bounded_buffer_when_public_fields_are_set_directly() {
    let zero_buffer = TransferOptions {
        buffer_size: 0,
        ..TransferOptions::default()
    };
    assert_eq!(zero_buffer.effective_buffer_size(), MIN_BUFFER_SIZE);

    let oversized_buffer = TransferOptions {
        buffer_size: usize::MAX,
        ..TransferOptions::default()
    };
    assert_eq!(oversized_buffer.effective_buffer_size(), MAX_BUFFER_SIZE);
}

#[test]
fn test_transfer_options_progress_callback() {
    let callback_invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag_clone = callback_invoked.clone();

    let opts = TransferOptions::default().with_progress_callback(move |_, _, _| {
        flag_clone.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    if let Some(ref cb) = opts.progress_callback {
        cb(1000, 5000, 1024.0);
    }
    assert!(callback_invoked.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn test_progress_percent_zero_total() {
    let prog = TransferProgress {
        bytes_transferred: 0,
        total_bytes: 0,
        speed_bytes_per_sec: 0.0,
        elapsed_secs: 0.0,
    };
    assert!((prog.percent() - 100.0).abs() < 0.001);
    assert!(prog.is_complete());
    assert_eq!(prog.remaining(), 0);
}

#[test]
fn test_progress_partial() {
    let prog = TransferProgress {
        bytes_transferred: 500,
        total_bytes: 1000,
        speed_bytes_per_sec: 250.0,
        elapsed_secs: 2.0,
    };
    assert!((prog.percent() - 50.0).abs() < 0.01);
    assert!(!prog.is_complete());
    assert_eq!(prog.remaining(), 500);

    let eta = prog.eta_secs();
    assert!(eta.is_some());
    assert!((eta.unwrap() - 2.0).abs() < 0.01);
}

#[test]
fn test_progress_complete() {
    let prog = TransferProgress {
        bytes_transferred: 1000,
        total_bytes: 1000,
        speed_bytes_per_sec: 500.0,
        elapsed_secs: 2.0,
    };
    assert!((prog.percent() - 100.0).abs() < 0.01);
    assert!(prog.is_complete());
    assert_eq!(prog.remaining(), 0);
    assert!(prog.eta_secs().is_none()); // No remaining = no ETA
}

#[test]
fn test_progress_display_format() {
    let prog = TransferProgress {
        bytes_transferred: 524288,     // 512KB
        total_bytes: 1048576,          // 1MB
        speed_bytes_per_sec: 262144.0, // 256KB/s
        elapsed_secs: 2.0,
    };
    let display = format!("{}", prog);
    assert!(display.contains("50.0%")); // ~50%
    assert!(display.contains("524288"));
    assert!(display.contains("256")); // KB/s
    assert!(display.contains("ETA"));
}

#[test]
fn test_progress_eta_with_zero_speed() {
    let prog = TransferProgress {
        bytes_transferred: 100,
        total_bytes: 10000,
        speed_bytes_per_sec: 0.0,
        elapsed_secs: 10.0,
    };
    assert!(prog.eta_secs().is_none()); // Cannot calculate ETA with zero speed
}

#[test]
fn test_constants() {
    assert_eq!(TRANSFER_BUF_SIZE, 65536); // 64KB
    assert_eq!(PROGRESS_REPORT_INTERVAL, 262144); // 256KB
    assert_eq!(MIN_BUFFER_SIZE, 1024); // 1KB
    assert_eq!(MAX_BUFFER_SIZE, 1048576); // 1MB
}
