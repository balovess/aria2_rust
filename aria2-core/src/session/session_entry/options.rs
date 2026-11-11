//! Download options conversion for session serialization
//!
//! Converts [`DownloadOptions`] struct to a `HashMap<String, String>` for
//! compact session file storage. Only non-default / non-empty values are
//! included; the load path uses `unwrap_or(default)` for every field, so
//! absent keys are safe.

use std::collections::HashMap;

use crate::request::request_group::DownloadOptions;

/// Converts DownloadOptions struct to a HashMap for serialization.
///
/// Only non-default / non-empty values are included to keep the session file
/// compact. The load path (`map_entry_to_download_options`) uses
/// `unwrap_or(default)` for every field, so absent keys are safe.
mod basic;
mod bittorrent;
mod integrity;
mod lifecycle;
mod network;

/// Convert typed download options to their session-file representation.
pub fn download_options_to_map(opts: &DownloadOptions) -> HashMap<String, String> {
    let mut map = HashMap::new();
    basic::apply(opts, &mut map);
    integrity::apply(opts, &mut map);
    bittorrent::apply(opts, &mut map);
    network::apply(opts, &mut map);
    lifecycle::apply(opts, &mut map);
    map
}

/// Add the small set of initial options whose original wire spelling must be
/// preserved alongside typed execution options.
pub fn download_options_to_map_with_snapshot(
    opts: &DownloadOptions,
    snapshot: Option<&HashMap<String, serde_json::Value>>,
) -> HashMap<String, String> {
    let mut map = download_options_to_map(opts);
    let Some(snapshot) = snapshot else {
        return map;
    };

    let registry = crate::config::OptionRegistry::new();
    for name in crate::config::INITIAL_SNAPSHOT_WIRE_OPTIONS {
        let Some(value) = snapshot
            .get(*name)
            .and_then(crate::request::request_group::option_value_to_string)
        else {
            continue;
        };

        let is_default = registry.get(name).is_some_and(|definition| {
            definition
                .parse_value(&value)
                .ok()
                .is_some_and(|parsed| parsed.to_string() == definition.default_value().to_string())
        });
        if !is_default {
            map.insert((*name).to_string(), value);
        }
    }
    map
}
