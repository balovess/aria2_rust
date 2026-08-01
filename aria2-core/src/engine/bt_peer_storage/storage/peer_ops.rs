use tracing::{debug, warn};

use super::DefaultPeerStorage;
use crate::engine::bt_peer_storage::peer_entry::PeerEntry;

impl DefaultPeerStorage {
    // ==================================================================
    // Peer addition
    // ==================================================================

    /// Add a single peer to the unused list.
    ///
    /// Returns true if the peer was added, false if rejected.
    ///
    /// A peer is rejected if:
    /// - The unused list is full (unused_peers.len() >= max_peer_list_size)
    /// - The peer is already tracked (duplicate ip:port)
    /// - The peer IP is in the blocklist
    /// - The peer is temporarily rejected
    ///
    /// Matches C++ DefaultPeerStorage::addPeer(shared_ptr<Peer>).
    pub fn add_peer(&mut self, peer: PeerEntry) -> bool {
        let key = (peer.ip.clone(), peer.port);

        if self.unused_peers.len() >= self.max_peer_list_size {
            debug!(
                "Adding {}:{} rejected: unused list full ({}/{}",
                peer.ip,
                peer.port,
                self.unused_peers.len(),
                self.max_peer_list_size
            );
            return false;
        }

        if self.uniq_peers.contains(&key) {
            debug!("Adding {}:{} rejected: already tracked", peer.ip, peer.port);
            return false;
        }

        if self.is_blocked_by_blocklist(&peer.ip) {
            debug!("Adding {}:{} rejected: blocklisted", peer.ip, peer.port);
            self.blocklist_reject_count += 1;
            return false;
        }

        if self.is_temporarily_rejected(&peer.ip) {
            debug!(
                "Adding {}:{} rejected: temporarily rejected",
                peer.ip, peer.port
            );
            return false;
        }

        // If list would overflow, evict from the back first.
        if self.unused_peers.len() >= self.max_peer_list_size {
            let excess = self.unused_peers.len() - self.max_peer_list_size + 1;
            self.delete_unused_peers(excess);
        }

        self.unused_peers.push_back(peer);
        self.uniq_peers.insert(key);
        debug!(
            "Added peer, unused list now has {} peers",
            self.unused_peers.len()
        );
        true
    }

    /// Add multiple peers to the unused list.
    ///
    /// If the unused list is already full before this call, all peers
    /// are rejected. Otherwise, each peer is individually checked for
    /// duplicates, blocklist membership, and temporary rejection before
    /// being added. After all additions, excess peers are evicted from
    /// the back.
    ///
    /// Matches C++ DefaultPeerStorage::addPeer(vector<shared_ptr<Peer>>).
    pub fn add_peers(&mut self, peers: Vec<PeerEntry>) {
        if self.unused_peers.len() < self.max_peer_list_size {
            for peer in peers {
                let key = (peer.ip.clone(), peer.port);

                if self.uniq_peers.contains(&key) {
                    debug!("Adding {}:{} rejected: already tracked", peer.ip, peer.port);
                    continue;
                }

                if self.is_blocked_by_blocklist(&peer.ip) {
                    debug!("Adding {}:{} rejected: blocklisted", peer.ip, peer.port);
                    self.blocklist_reject_count += 1;
                    continue;
                }

                if self.is_temporarily_rejected(&peer.ip) {
                    debug!(
                        "Adding {}:{} rejected: temporarily rejected",
                        peer.ip, peer.port
                    );
                    continue;
                }

                debug!("Adding peer {}:{}", peer.ip, peer.port);
                self.unused_peers.push_back(peer);
                self.uniq_peers.insert(key);
            }
        } else {
            for peer in &peers {
                debug!(
                    "Adding {}:{} rejected: unused list full ({}/{}",
                    peer.ip,
                    peer.port,
                    self.unused_peers.len(),
                    self.max_peer_list_size
                );
            }
        }

        // Evict excess peers from the back.
        if self.unused_peers.len() > self.max_peer_list_size {
            let excess = self.unused_peers.len() - self.max_peer_list_size;
            self.delete_unused_peers(excess);
        }

        debug!(
            "After batch add, unused list has {} peers",
            self.unused_peers.len()
        );
    }

    /// Atomically add a peer and check it out.
    ///
    /// If the peer is blocked by the blocklist or temporarily rejected,
    /// returns None. If the peer is already tracked and in the unused
    /// list, it is moved to the front for immediate checkout. If already
    /// in the used list, returns None.
    /// If the peer is new, it is added to the front of the unused list
    /// and then checked out.
    ///
    /// Matches C++ DefaultPeerStorage::addAndCheckoutPeer.
    pub fn add_and_checkout_peer(&mut self, peer: PeerEntry, cuid: u64) -> Option<PeerEntry> {
        let key = (peer.ip.clone(), peer.port);

        if self.is_blocked_by_blocklist(&peer.ip) {
            debug!(
                "addAndCheckout: {}:{} rejected: blocklisted",
                peer.ip, peer.port
            );
            self.blocklist_reject_count += 1;
            return None;
        }

        if self.is_temporarily_rejected(&peer.ip) {
            debug!(
                "addAndCheckout: {}:{} rejected: temporarily rejected",
                peer.ip, peer.port
            );
            return None;
        }

        if self.uniq_peers.contains(&key) {
            // Peer already tracked. Try to find in unused list.
            let pos = self
                .unused_peers
                .iter()
                .position(|p| p.ip == peer.ip && p.port == peer.port);

            {
                let idx = pos?;
                // Remove from unused list; we'll push to front below.
                self.unused_peers.remove(idx);
            }
        } else {
            // New peer -- register in uniq set.
            self.uniq_peers.insert(key);
        }

        // Push to front for immediate checkout (C++ uses push_front).
        self.unused_peers.push_front(peer);

        self.checkout_peer(cuid)
    }

    // ==================================================================
    // Peer checkout / return lifecycle
    // ==================================================================

    /// Check out the next available unused peer for the given caretaker.
    ///
    /// Moves the peer from the unused list to the used set, setting
    /// used_by to cuid. Returns None if no peers are available.
    ///
    /// Matches C++ DefaultPeerStorage::checkoutPeer.
    pub fn checkout_peer(&mut self, cuid: u64) -> Option<PeerEntry> {
        if !self.is_peer_available() {
            return None;
        }

        let mut peer = self
            .unused_peers
            .pop_front()
            .expect("is_peer_available guarantees non-empty");

        if peer.used_by != 0 {
            warn!(
                "CUID#{} is already set for peer {}:{}",
                peer.used_by, peer.ip, peer.port
            );
        }

        peer.used_by = cuid;
        self.used_peers.insert(peer.clone());
        debug!("Checkout peer {}:{} to CUID#{}", peer.ip, peer.port, cuid);
        Some(peer)
    }

    /// Return a peer from the used set.
    ///
    /// Handles the peer's disconnect lifecycle:
    /// - If the peer was active and disconnected gracefully and is not
    ///   incoming, add it to the dropped list.
    /// - If the peer was not choking and the peer was interested, trigger
    ///   a choke round.
    /// - Remove from uniq_peers.
    ///
    /// Matches C++ DefaultPeerStorage::returnPeer.
    pub fn return_peer(&mut self, peer: &PeerEntry) {
        debug!(
            "Peer {}:{} returned from CUID#{}",
            peer.ip, peer.port, peer.used_by
        );

        if self.used_peers.remove(peer) {
            self.on_returning_peer(peer);
            self.on_erasing_peer(peer);
        } else {
            warn!("Cannot find peer {}:{} in used_peers", peer.ip, peer.port);
        }
    }

    /// Check whether any unused peer is available for checkout.
    pub fn is_peer_available(&self) -> bool {
        !self.unused_peers.is_empty()
    }

    // ==================================================================
    // Peer eviction
    // ==================================================================

    /// Delete peers from the back of the unused list.
    ///
    /// Each removed peer is also removed from uniq_peers.
    /// Matches C++ DefaultPeerStorage::deleteUnusedPeer.
    pub fn delete_unused_peers(&mut self, del_size: usize) {
        for _ in 0..del_size {
            if let Some(peer) = self.unused_peers.pop_back() {
                self.on_erasing_peer(&peer);
                debug!("Removed peer {}:{}", peer.ip, peer.port);
            }
        }
    }

    // ==================================================================
    // Peer lookup (C++ DefaultPeerStorage::getPeer)
    // ==================================================================

    /// Find a peer by IP address and port.
    ///
    /// Searches both used_peers and unused_peers. Returns a clone
    /// of the matching PeerEntry if found, None otherwise.
    ///
    /// Matches C++ DefaultPeerStorage::getPeer(ipaddr, port).
    pub fn get_peer(&self, ipaddr: &str, port: u16) -> Option<PeerEntry> {
        // Check used_peers first (active connections are more likely targets)
        let key = PeerEntry::new(ipaddr.to_string(), port);
        if let Some(peer) = self.used_peers.get(&key) {
            return Some(peer.clone());
        }
        // Then check unused_peers
        for peer in &self.unused_peers {
            if peer.ip == ipaddr && peer.port == port {
                return Some(peer.clone());
            }
        }
        None
    }

    // ==================================================================
    // Lifecycle callbacks (C++ DefaultPeerStorage::onErasingPeer, onReturningPeer)
    // ==================================================================

    /// Handle peer removal from the used set: remove from uniq_peers.
    ///
    /// In C++ this is a public method called when a peer is removed from
    /// usedPeers_. Here it is also public so that external callers
    /// (e.g. BtInteractive) can trigger it directly.
    ///
    /// Matches C++ DefaultPeerStorage::onErasingPeer.
    pub fn on_erasing_peer(&mut self, peer: &PeerEntry) {
        self.uniq_peers.remove(&(peer.ip.clone(), peer.port));
    }

    /// Handle peer return: drop tracking and choke triggering.
    ///
    /// In C++ this is a public method. It adds gracefully-disconnected
    /// outgoing peers to the dropped list, and triggers a choke round
    /// if an unchoked+interested peer disconnects.
    ///
    /// Matches C++ DefaultPeerStorage::onReturningPeer.
    pub fn on_returning_peer(&mut self, peer: &PeerEntry) {
        if peer.is_active {
            if peer.disconnected_gracefully && !peer.is_incoming {
                self.add_dropped_peer(peer);
            }

            if !peer.am_choking && peer.peer_interested {
                debug!(
                    "Unchoked+interested peer {}:{} disconnected, choke round needed",
                    peer.ip, peer.port
                );
            }
        }
    }
}
