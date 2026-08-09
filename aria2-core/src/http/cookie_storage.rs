//! Backward-compatible re-exports from the cookie module.
//!
//! This module exists to maintain backward compatibility for code that imports
//! `crate::http::cookie_storage::{CookieStorage, CookieJar, JarCookie}`.
//! New code should import directly from `crate::http::cookie` instead.

pub use crate::http::cookie::{CookieJar, CookieStorage, JarCookie};
