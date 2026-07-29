//! Unchoke candidate selection logic (tit-for-tat rotation)

use super::{ChokeAction, ChokingAlgorithm};
use crate::constants;

/// Core algorithm: performs tit-for-tat choke rotation.
///
/// Steps:
/// 1. Check and mark snubbed peers (timeout-based)
/// 2. Calculate score for each peer
/// 3. Sort by score descending
/// 4. Top K get Unchoke, rest get Choke
///    BUT: keep currently unchoked peers unchoked if they're still in top K
///    (avoid churn - only change what's necessary)
/// 5. Return only the actions that changed state
pub(super) fn rotate_choke(algo: &mut ChokingAlgorithm) -> Vec<ChokeAction> {
    // Step 1: Check and mark snubbed peers
    check_snubbed_peers_internal(algo);

    if algo.peers.is_empty() {
        return vec![];
    }

    let max_slots = algo.config.max_upload_slots;

    // Step 2: Calculate scores and sort indices by score descending
    let mut scored_peers: Vec<(usize, f64)> = algo
        .peers
        .iter()
        .enumerate()
        .map(|(i, peer)| {
            let is_snubbed = algo.snubbed_peers.contains(&i);
            (i, calculate_peer_score(peer, is_snubbed))
        })
        .collect();

    // Sort by score descending
    scored_peers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Step 3 & 4: Determine which peers should be unchoked vs choked
    let mut actions = Vec::new();
    let mut _new_unchoked_indices = std::collections::HashSet::new();

    // Top K peers should be unchoked
    for (rank, &(idx, _)) in scored_peers.iter().enumerate() {
        if rank < max_slots {
            // Should be unchoked
            if algo.peers[idx].am_choking {
                actions.push(ChokeAction::Unchoke(idx));
                algo.peers[idx].record_unchoke();
            } else {
                actions.push(ChokeAction::NoChange(idx));
            }
            _new_unchoked_indices.insert(idx);
        } else {
            // Should be choked
            if !algo.peers[idx].am_choking {
                actions.push(ChokeAction::Choke(idx));
                algo.peers[idx].record_choke();
            } else {
                actions.push(ChokeAction::NoChange(idx));
            }
        }
    }

    actions
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
