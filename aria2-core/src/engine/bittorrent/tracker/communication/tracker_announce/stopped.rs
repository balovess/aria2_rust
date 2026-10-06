use super::*;

impl TrackerAnnouncer {
    /// Send a "stopped" event to all trackers before shutdown.
    ///
    /// C++ aria2 sends stopped events during `DownloadEngine::setHaltRequested()`.
    /// This should be called before the download command exits.
    pub async fn announce_stopped(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
    ) {
        if self.stopped_sent {
            return;
        }
        self.announce.set_runtime_halted(true);
        self.publish_runtime_snapshot();

        // Try to send stopped events to all applicable tiers.
        let mut attempts = 0;
        const MAX_STOPPED_ATTEMPTS: usize = 5;
        let mut sent_successfully = false;

        let deadline = Instant::now() + self.stopped_timeout;
        while self.announce.is_stopped_announce_ready() && attempts < MAX_STOPPED_ATTEMPTS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!(
                    "[BT] Stopped announce budget exhausted after {} attempts",
                    attempts
                );
                break;
            }
            let result = match tokio::time::timeout(
                remaining,
                self.announce(info_hash, peer_id, downloaded, left, uploaded),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    warn!(
                        "[BT] Stopped announce timed out after {:?}",
                        self.stopped_timeout
                    );
                    break;
                }
            };
            if let Some(result) = result {
                info!(
                    "[BT] Sent stopped event to {} ({} peers in response)",
                    result.tracker_url,
                    result.peers.len()
                );
                sent_successfully = true;
            }
            attempts += 1;
        }
        self.stopped_sent = sent_successfully;
    }

    /// Send a "completed" event to all applicable trackers.
    ///
    /// Called when the download finishes all pieces.
    pub async fn announce_completed(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        uploaded: u64,
    ) {
        self.announce.set_download_complete(true);
        self.publish_runtime_snapshot();

        if let Some(result) = self
            .announce(info_hash, peer_id, downloaded, 0, uploaded)
            .await
        {
            info!(
                "[BT] Sent completed event to {} ({:?} seeders, {:?} leechers)",
                result.tracker_url, result.seeders, result.leechers
            );
        }
    }
}
