//! Unchoke candidate selection logic (tit-for-tat rotation)

use super::{ChokingAlgorithm, IdentityChokeAction, PeerIdentity};
use crate::constants;

/// Core algorithm: performs tit-for-tat choke rotation.
///
/// Steps:
/// 1. Check and mark snubbed peers (timeout-based)
/// 2. Calculate score for each peer
/// 3. Sort by score descending
/// 4. Interested, non-snubbed peers compete for regular slots; the configured
///    upload-slot limit includes one separately managed optimistic slot
///    BUT: keep currently unchoked peers unchoked if they're still in top K
///    (avoid churn - only change what's necessary)
/// 5. Return only the actions that changed state
pub(super) fn rotate_choke_by_identity(algo: &mut ChokingAlgorithm) -> Vec<IdentityChokeAction> {
    check_snubbed_peers_internal(algo);
    if algo.peers.is_empty() {
        return Vec::new();
    }
    let regular_slots = algo.config.max_upload_slots.saturating_sub(1);
    let mut scored: Vec<(PeerIdentity, bool, f64)> = algo
        .peers
        .iter()
        .filter(|peer| {
            peer.peer_interested
                && !peer.is_snubbed
                && !algo.snubbed_peers.contains(&PeerIdentity::from(*peer))
        })
        .map(|peer| {
            let identity = peer.into();
            (
                identity,
                peer.last_data_time
                    .is_some_and(|received_at| received_at.elapsed().as_secs() < 30),
                calculate_peer_score(peer, algo.snubbed_peers.contains(&identity)),
            )
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    let selected: std::collections::HashSet<_> = scored
        .into_iter()
        .take(regular_slots)
        .map(|(identity, _, _)| identity)
        .collect();

    algo.peers
        .iter_mut()
        .map(|peer| {
            let identity = PeerIdentity::from(&*peer);
            if selected.contains(&identity) {
                if peer.am_choking {
                    peer.record_unchoke();
                    IdentityChokeAction::Unchoke(identity)
                } else {
                    IdentityChokeAction::NoChange(identity)
                }
            } else if !peer.am_choking {
                peer.record_choke();
                IdentityChokeAction::Choke(identity)
            } else {
                IdentityChokeAction::NoChange(identity)
            }
        })
        .collect()
}

/// Internal implementation of snubbed checking.
///
/// Iterates through all peers and marks those that have exceeded
/// the snubbed timeout as snubbed via PeerStats::check_snubbed.
///
/// Returns indices of newly snubbed peers.
pub(super) fn check_snubbed_peers_internal(algo: &mut ChokingAlgorithm) -> Vec<usize> {
    let mut snubbed = vec![];
    for (i, peer) in algo.peers.iter_mut().enumerate() {
        if peer.check_snubbed(algo.config.snubbed_timeout_secs) {
            algo.snubbed_peers.insert((&*peer).into());
            snubbed.push(i);
        }
    }
    snubbed
}

/// Score function: higher = better peer to keep unchoked.
///
/// Score components:
///   1. Download speed contribution (how much they give us): weight 0.5
///   2. Upload speed contribution (reciprocity): weight 0.3
///   3. Snubbed penalty: -1000 if snubbed (either in PeerStats or algorithm set)
///   4. Interest bonus: +50 if peer_interested
///   5. New peer bonus (time since unchoke < 60s): +30 (anti-churn)
pub(super) fn calculate_peer_score(
    peer: &crate::engine::peer_stats::PeerStats,
    is_explicitly_snubbed: bool,
) -> f64 {
    let mut score = 0.0;

    // Download speed (primary factor - tit-for-tat)
    // Scale down to reasonable range
    score += peer.download_speed * constants::CHOKING_DOWNLOAD_SPEED_WEIGHT;

    // Upload speed (reciprocity)
    score += peer.upload_speed * constants::CHOKING_UPLOAD_SPEED_WEIGHT;

    // Snubbed penalty (heavy penalty to avoid wasting slots)
    // Check both PeerStats-level and algorithm-level snubbing
    if peer.is_snubbed || is_explicitly_snubbed {
        score -= constants::CHOKING_SNUBBED_PENALTY;
    }

    // Interest bonus (prefer peers who want our data)
    if peer.peer_interested {
        score += constants::CHOKING_INTEREST_BONUS;
    }

    // Anti-churn: prefer keeping current unchoked peers stable
    if !peer.am_choking
        && peer.time_since_last_unchoke().as_secs() < constants::CHOKING_ANTI_CHURN_THRESHOLD_SECS
    {
        score += constants::CHOKING_ANTI_CHURN_BONUS;
    }

    score
}
