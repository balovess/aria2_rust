use std::sync::Arc;
use std::time::Instant;

use crate::engine::bittorrent::peer::stats::SwarmUploadRate;
use crate::request::request_group::AtomicProgress;

/// Publish the swarm's upload rate on new payload events and sample expiry.
pub(crate) fn spawn(
    progress: Arc<AtomicProgress>,
    upload_rate: Arc<SwarmUploadRate>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let changed = upload_rate.changed().notified();
            tokio::pin!(changed);
            changed.as_mut().enable();

            let now = Instant::now();
            progress.set_upload_speed(upload_rate.speed_at(now));
            if let Some(deadline) = upload_rate.next_expiration_after(now) {
                tokio::select! {
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                    _ = &mut changed => {}
                }
            } else {
                changed.await;
            }
        }
    })
}
