use std::sync::OnceLock;
use tracing::Level;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    layer::{Layer, SubscriberExt},
    util::SubscriberInitExt,
};

static LOG_GUARD: OnceLock<Vec<tracing_appender::non_blocking::WorkerGuard>> = OnceLock::new();

fn parse_log_level(level_str: &str) -> Level {
    match level_str.to_lowercase().as_str() {
        "debug" => Level::DEBUG,
        "info" | "notice" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    }
}

pub fn init_logging(
    log_level: &str,
    console_log_level: &str,
    log_file: Option<&str>,
    log_backup_count: usize,
) {
    let file_level = parse_log_level(log_level);
    let console_level = parse_log_level(console_log_level);

    let enable_file_logging = log_file.is_some_and(|f| f != "-");

    if enable_file_logging {
        let path = log_file.unwrap();
        let p = std::path::Path::new(path);
        let dir = p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string());
        let stem = p.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "aria2.log".to_string());

        use tracing_appender::non_blocking;
        use tracing_appender::rolling::Rotation;
        let file_appender = tracing_appender::rolling::Builder::new()
            .rotation(Rotation::DAILY)
            .filename_prefix(&stem)
            .max_log_files(log_backup_count)
            .build(&dir)
            .unwrap_or_else(|_| {
                tracing_appender::rolling::daily(&dir, &stem)
            });
        let (non_blocking, guard) = non_blocking(file_appender);
        let _ = LOG_GUARD.set(vec![guard]);

        let file_filter = EnvFilter::from_default_env()
            .add_directive(file_level.into())
            .add_directive("hyper=warn".parse().unwrap())
            .add_directive("reqwest=warn".parse().unwrap());

        let console_filter = EnvFilter::from_default_env()
            .add_directive(console_level.into())
            .add_directive("hyper=warn".parse().unwrap())
            .add_directive("reqwest=warn".parse().unwrap());

        let console_layer = fmt::layer()
            .with_span_events(FmtSpan::CLOSE)
            .with_target(false)
            .with_filter(console_filter);

        let file_layer = fmt::Layer::new()
            .with_span_events(FmtSpan::CLOSE)
            .with_target(false)
            .with_writer(non_blocking)
            .with_filter(file_filter);

        let _ = tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .try_init();
    } else {
        let env_filter = EnvFilter::from_default_env()
            .add_directive(console_level.into())
            .add_directive("hyper=warn".parse().unwrap())
            .add_directive("reqwest=warn".parse().unwrap());

        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer()
                .with_span_events(FmtSpan::CLOSE)
                .with_target(false))
            .try_init();
    }

    tracing::info!("Log system initialization complete");
}
