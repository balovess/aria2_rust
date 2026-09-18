use crate::error::{Aria2Error, RecoverableError};
use tokio::sync::mpsc;

use super::*;
use crate::http::auth::AuthResolveOptions;
use std::time::Duration;

fn has_header(request: &str, name: &str, value: &str) -> bool {
    request.lines().any(|line| {
        line.split_once(':')
            .is_some_and(|(key, actual)| key.eq_ignore_ascii_case(name) && actual.trim() == value)
    })
}

fn has_header_name(request: &str, name: &str) -> bool {
    request.lines().any(|line| {
        line.split_once(':')
            .is_some_and(|(key, _)| key.eq_ignore_ascii_case(name))
    })
}

fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        line.split_once(':')
            .and_then(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.trim()))
    })
}

fn digest_parameter<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .strip_prefix("Digest ")
        .and_then(|parameters| {
            parameters.split(", ").find_map(|parameter| {
                parameter
                    .split_once('=')
                    .and_then(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value))
            })
        })
        .map(|value| value.trim_matches('"'))
}

mod auth;
mod basic;
mod streaming;
