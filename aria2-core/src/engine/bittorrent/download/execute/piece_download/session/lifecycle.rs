use crate::error::Result;

use super::PieceDownloadSession;

impl PieceDownloadSession<'_> {
    pub(super) async fn run(self) -> Result<()> {
        let upload_speed_reporter =
            crate::engine::bittorrent::download::execute::spawn_upload_speed_reporter(
                std::sync::Arc::clone(&self.command.progress),
                self.swarm.upload_rate(),
            );
        let result = self.run_loop().await;
        upload_speed_reporter.abort();
        let _ = upload_speed_reporter.await;
        result
    }
}
