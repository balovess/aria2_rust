//! Optimistic unchoke logic (round-robin rotation)

use super::{ChokingAlgorithm, PeerIdentity};

pub(super) fn optimistically_unchoke_by_identity(
    algo: &mut ChokingAlgorithm,
) -> Option<PeerIdentity> {
    // Find candidates that are:
    //   - Currently choked (am_choking == true)
    //   - Interested in us (peer_interested == true)
    //   - Not snubbed (neither PeerStats.is_snubbed nor in explicit set)
    //   - Not recently optimistically unchoked (>interval ago)
    let candidates: Vec<PeerIdentity> = algo
        .peers
        .iter()
        .enumerate()
        .filter(|(_, peer)| {
            peer.am_choking
                && peer.peer_interested
                && !peer.is_snubbed
                && !algo.snubbed_peers.contains(&(*peer).into())
                && peer.time_since_last_optimistic_unchoke().as_secs()
                    >= algo.config.optimistic_unchoke_interval_secs
        })
        .map(|(_, peer)| PeerIdentity::from(peer))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Use round-robin selection: pick next candidate after current position
    let selected = rotate_optimistic_unchoked(algo, &candidates);

    // Mark as optimistically unchoked
    if let Some(peer) = algo
        .peers
        .iter_mut()
        .find(|peer| PeerIdentity::from(&**peer) == selected)
    {
        peer.record_optimistic_unchoke();
    }
    algo.current_optimistic_peer = Some(selected);

    Some(selected)
}

/// Rotate which peer gets the optimistic unchoke slot using round-robin.
///
/// Picks a different peer than the current one when possible,
/// cycling through eligible peers in order.
///
/// # Arguments
/// * algo - The choking algorithm (for accessing rotation state)
/// * eligible_peers - Identities of peers that are eligible for optimistic unchoke
///
/// # Returns
/// The selected peer identity
///
/// # Panics
/// Panics if eligible_peers is empty.
pub(super) fn rotate_optimistic_unchoked(
    algo: &mut ChokingAlgorithm,
    eligible_peers: &[PeerIdentity],
) -> PeerIdentity {
    if eligible_peers.is_empty() {
        panic!("rotate_optimistic_unchoked called with empty eligible list");
    }

    if eligible_peers.len() == 1 {
        return eligible_peers[0];
    }

    // Find position of current optimistic peer in eligible list
    let current_pos = algo
        .current_optimistic_peer
        .and_then(|curr| eligible_peers.iter().position(|identity| *identity == curr));

    // Advance to next peer in round-robin order
    let next_pos = match current_pos {
        Some(pos) => (pos + 1) % eligible_peers.len(),
        None => algo.optimistic_rotation_counter % eligible_peers.len(),
    };

    algo.optimistic_rotation_counter = algo.optimistic_rotation_counter.wrapping_add(1);
    eligible_peers[next_pos]
}
