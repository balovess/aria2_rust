//! aria2-rust CLI library
//!
//! This crate provides the command-line interface components for aria2-rust,
//! including the TUI progress bar system and display utilities.

pub mod app;
pub mod constants;
pub mod daemon;
pub mod identity;
pub mod ui;

pub use daemon::{DaemonConfig, DaemonError, Daemonizer, PidFileGuard, PidFileManager};
pub use ui::progress_bar;
