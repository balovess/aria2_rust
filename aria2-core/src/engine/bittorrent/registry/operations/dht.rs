//! BitTorrent registry dht operations.

use super::super::BtRegistry;
use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use std::sync::Arc;
use tracing::trace;

impl BtRegistry {
    // -----------------------------------------------------------------------
    // DHT engine management
    // -----------------------------------------------------------------------

    /// Set the process-wide DHT engine for its bound address family.
    pub fn set_global_dht_engine(
        &mut self,
        engine: Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) {
        trace!("BtRegistry::set_global_dht_engine");
        self.global_dht_engines.insert(engine);
    }

    pub(crate) fn attach_global_dht_engine_for_gid(
        &mut self,
        gid: u64,
        use_ipv6: bool,
    ) -> Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        let engine = if use_ipv6 {
            self.global_dht_engines.ipv6()
        } else {
            self.global_dht_engines.ipv4()
        }?;
        self.set_dht_engine_for_gid(gid, Arc::clone(&engine));
        Some(engine)
    }

    /// Install the process DHT engine only if one has not already been
    /// published, returning the canonical shared handle in either case.
    ///
    /// Callers hold the registry write lock across this operation, so
    /// concurrent first-use startup converges on one engine rather than
    /// replacing the process engine with whichever startup finishes last.
    pub fn get_or_install_global_dht_engine(
        &mut self,
        candidate: Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) -> Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine> {
        if let Some(engine) = self.global_dht_engines.for_peer(candidate.local_addr()) {
            return engine;
        }

        self.global_dht_engines.insert(Arc::clone(&candidate));
        candidate
    }

    /// Start the process DHT engine for this configuration's address family,
    /// or attach the download to the already-running family engine.
    ///
    /// The family startup gate is acquired without holding the registry lock,
    /// then the registry is checked again before binding a UDP socket. This
    /// keeps concurrent torrent starts from creating throwaway DHT engines.
    pub async fn get_or_start_global_dht_engine(
        registry: &Arc<std::sync::RwLock<Self>>,
        gid: u64,
        config: aria2_protocol::bittorrent::dht::engine::DhtEngineConfig,
    ) -> std::io::Result<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        let use_ipv6 = config.listen_addr.is_some_and(|address| address.is_ipv6());
        let startup_lock = {
            let registry = registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(&registry.dht_engine_start_locks[usize::from(use_ipv6)])
        };
        let startup_guard = startup_lock.lock().await;

        let existing = {
            let registry = registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if use_ipv6 {
                registry.global_dht_engines.ipv6()
            } else {
                registry.global_dht_engines.ipv4()
            }
        };
        if let Some(engine) = existing {
            registry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_dht_engine_for_gid(gid, Arc::clone(&engine));
            return Ok(engine);
        }

        let registry_weak = Arc::downgrade(registry);
        let candidate =
            aria2_protocol::bittorrent::dht::engine::DhtEngine::start_with_local_peer_lookup(
                config,
                move |info_hash, requester_ip| {
                    let registry = registry_weak.upgrade()?;
                    let registry = registry
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    registry.dht_local_peer(info_hash, requester_ip)
                },
            )
            .await?;
        let engine = {
            let mut registry = registry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let engine = registry.get_or_install_global_dht_engine(Arc::clone(&candidate));
            registry.set_dht_engine_for_gid(gid, Arc::clone(&engine));
            engine
        };

        drop(startup_guard);
        if !Arc::ptr_eq(&candidate, &engine) {
            candidate.shutdown_async().await;
        }
        Ok(engine)
    }

    /// Get the process-wide DHT engine that can contact this peer address.
    pub fn get_global_dht_engine_for_peer(
        &self,
        address: std::net::SocketAddr,
    ) -> Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        self.global_dht_engines.for_peer(address)
    }

    /// Return whether a command handle refers to the process-wide engine.
    pub fn is_global_dht_engine(
        &self,
        engine: &Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) -> bool {
        self.global_dht_engines
            .iter()
            .any(|current| Arc::ptr_eq(current, engine))
    }

    /// Take all process engines during final session shutdown.
    pub fn take_global_dht_engines(
        &mut self,
    ) -> Vec<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        std::mem::take(&mut self.global_dht_engines).into_vec()
    }

    /// Register the DHT engine owned by one active download.
    pub fn set_dht_engine_for_gid(
        &mut self,
        gid: u64,
        engine: Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) {
        trace!(gid, "BtRegistry::set_dht_engine_for_gid");
        self.dht_engines.entry(gid).or_default().insert(engine);
    }

    /// Remove one download's DHT engine only when the handle still matches.
    pub fn clear_dht_engine_for_gid_if(
        &mut self,
        gid: u64,
        engine: &Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) {
        if let Some(engines) = self.dht_engines.get_mut(&gid) {
            engines.remove_if(engine);
            if engines.is_empty() {
                self.dht_engines.remove(&gid);
            }
        }
    }

    /// Clone all currently registered DHT engines for a status snapshot.
    pub fn get_dht_engines(&self) -> Vec<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        let mut engines = Vec::with_capacity(self.dht_engines.len() * 2 + 2);
        for engine in self
            .global_dht_engines
            .iter()
            .chain(self.dht_engines.values().flat_map(DhtEngineSet::iter))
        {
            if !engines.iter().any(|current| Arc::ptr_eq(current, engine)) {
                engines.push(Arc::clone(engine));
            }
        }
        engines
    }

    // -----------------------------------------------------------------------
}
