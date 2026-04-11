use tracing::Level;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

pub fn init_logging(level: Level, log_file: Option<&str>) {
    let env_filter = EnvFilter::from_default_env()
        .add_directive(level.into())
        .add_directive("hyper=warn".parse().unwrap())
        .add_directive("reqwest=warn".parse().unwrap());

    let fmt_layer = fmt::layer()
        .with_span_events(FmtSpan::CLOSE)
        .with_target(false);

    if let Some(_file) = log_file {
        use tracing_appender::non_blocking;
        let file_appender = tracing_appender::rolling::daily(".", "aria2.log");
        let (non_blocking, _guard) = non_blocking(file_appender);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .with(fmt::Layer::new().with_writer(non_blocking))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .init();
    }

    tracing::info!("日志系统初始化完成");
}
