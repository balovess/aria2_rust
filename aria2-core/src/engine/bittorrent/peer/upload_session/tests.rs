use super::*;

#[derive(Default)]
struct TestUploadTransport {
    sent: Vec<BtMessage>,
    supports_fast_extension: bool,
    am_allowed_fast: std::collections::HashSet<u32>,
}

#[async_trait]
impl BtUploadTransport for TestUploadTransport {
    fn supports_fast_extension(&self) -> bool {
        self.supports_fast_extension
    }

    fn am_allowed_fast(&self, piece_index: u32) -> bool {
        self.am_allowed_fast.contains(&piece_index)
    }

    async fn send_upload_message(
        &mut self,
        message: &BtMessage,
    ) -> std::result::Result<(), String> {
        self.sent.push(message.clone());
        Ok(())
    }

    async fn send_upload_choke(&mut self) -> std::result::Result<(), String> {
        self.sent.push(BtMessage::Choke);
        Ok(())
    }

    async fn send_upload_unchoke(&mut self) -> std::result::Result<(), String> {
        self.sent.push(BtMessage::Unchoke);
        Ok(())
    }
}

#[path = "tests/provider.rs"]
mod provider;
#[path = "tests/state.rs"]
mod state;
