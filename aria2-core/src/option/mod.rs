//! Option handling module.
//!
//! Provides [`OptionHandler`] -- a centralized option management struct with
//! built-in C++ aria2 compatible defaults, `.aria2rc` config file parsing,
//! CLI argument override support, and conversion to [`DownloadOptions`].

pub mod option_handler;

pub use option_handler::{OptionHandler, OptionValue};
