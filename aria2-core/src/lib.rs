pub mod error;
pub mod log;
pub mod colorized_stream;
pub mod engine;
pub mod request;
pub mod segment;
pub mod filesystem;
pub mod config;
pub mod util;

use tracing::Level;

pub fn init_logging(level: Level, log_file: Option<&str>) {
    log::init_logging(level, log_file);
}
