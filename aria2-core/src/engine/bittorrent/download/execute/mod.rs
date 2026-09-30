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
mod runtime_tests;
#[cfg(test)]
mod tests;
mod tracker_actor;
pub mod types;

// Keep the download-execution interface at this module root for its callers.
pub(crate) use tracker_actor::BtTrackerAnnouncerActor;
pub use types::EndgameState;

pub(crate) use dht_periodic_lookup::{DhtPeriodicLookup, check_periodic_dht_lookup};
pub(crate) use pex::{PEX_SEND_INTERVAL, send_periodic_pex_to_swarm};

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
