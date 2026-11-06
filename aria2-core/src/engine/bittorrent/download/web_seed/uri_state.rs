use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::warn;

#[derive(Default)]
pub(super) struct WebSeedUriState {
    state: std::sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    uri_generation: Option<u64>,
    unavailable: HashSet<String>,
    probed: HashSet<String>,
    probes: HashMap<(Option<u64>, String), Arc<tokio::sync::Mutex<()>>>,
}

pub(super) enum UriProbeAdmission {
    Unavailable,
    Proceed,
    Probe(tokio::sync::OwnedMutexGuard<()>),
}

impl WebSeedUriState {
    pub(super) fn observe_generation(&self, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .uri_generation
            .is_none_or(|observed| generation > observed)
        {
            state.unavailable.clear();
            state.probed.clear();
            state.probes.clear();
            state.uri_generation = Some(generation);
        }
    }

    pub(super) async fn admit_request(
        &self,
        uri: &str,
        generation: Option<u64>,
    ) -> UriProbeAdmission {
        if self.is_unavailable(uri, generation) {
            return UriProbeAdmission::Unavailable;
        }
        if self.is_probed(uri, generation) {
            return UriProbeAdmission::Proceed;
        }

        let probe = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(
                state
                    .probes
                    .entry((generation, uri.to_string()))
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        let guard = probe.lock_owned().await;
        if self.is_unavailable(uri, generation) {
            UriProbeAdmission::Unavailable
        } else if self.is_probed(uri, generation) {
            UriProbeAdmission::Proceed
        } else {
            UriProbeAdmission::Probe(guard)
        }
    }

    pub(super) fn mark_unavailable(&self, uri: &str, generation: Option<u64>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.uri_generation == generation {
            state.probed.remove(uri);
            state.unavailable.insert(uri.to_string());
            warn!(
                url = uri,
                "Web-seed returned HTTP 404; skipping it until URI configuration changes"
            );
        }
    }

    pub(super) fn mark_probed(&self, uri: &str, generation: Option<u64>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.uri_generation == generation {
            state.probed.insert(uri.to_string());
        }
    }

    fn is_unavailable(&self, uri: &str, generation: Option<u64>) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.uri_generation == generation && state.unavailable.contains(uri)
    }

    fn is_probed(&self, uri: &str, generation: Option<u64>) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.uri_generation == generation && state.probed.contains(uri)
    }
}
