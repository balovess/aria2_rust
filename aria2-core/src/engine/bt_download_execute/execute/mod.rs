mod bep6;
mod checkpoint;
mod command;
mod dht_periodic_lookup;
mod environment;
mod finalization;
mod hash_verification;
mod incoming;
mod peer_management;
mod peer_session;
mod pex;
mod piece_download;
mod registry;
mod state;
mod web_seed;

#[cfg(test)]
mod tests;

pub use dht_periodic_lookup::{DhtPeriodicLookup, check_periodic_dht_lookup};

use std::collections::HashSet;

pub(crate) fn deduplicate_tracker_tiers(tiers: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let mut seen = HashSet::new();
    tiers
        .into_iter()
        .filter_map(|tier| {
            let unique = tier
                .into_iter()
                .filter(|url| seen.insert(url.clone()))
                .collect::<Vec<_>>();
            (!unique.is_empty()).then_some(unique)
        })
        .collect()
}
