//! Message reception loop for `BtPeerInteractive`.
//!
//! Contains [eceive_messages()] which reads all available messages
//! from the peer connection and dispatches each one.

use std::time::Instant;

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::extension_registry::ExtensionUpdate;
use crate::error::Result;
use tracing::{trace, warn};

use super::super::types::*;
use super::BtPeerInteractive;

impl BtPeerInteractive {
    /// Receive messages from the peer connection and dispatch each one.
    ///
    /// Mirrors C++ `receiveMessages()`: reads all available messages
    /// from the peer, dispatches each to the handler via
    /// [dispatch_message()], and resets the inactive timer on data
    /// messages.
    ///
    /// Returns the number of messages received and the last inbound
    /// PEX `ExtensionUpdate` (if any) so the caller can add discovered
    /// peers to the known-peers list.
    pub(crate) async fn receive_messages<F>(
        &mut self,
        conn: &mut BtPeerConn,
        is_in_allowed_fast: F,
    ) -> Result<(usize, Option<ExtensionUpdate>)>
    where
        F: Fn(u32) -> bool,
    {
        let mut count = 0usize;
        let mut last_pex_update: Option<ExtensionUpdate> = None;

        // Read up to a reasonable batch of messages per iteration.
        // The C++ code reads in a loop while messages are available.
        for _ in 0..UB_MAX_OUTSTANDING_REQUEST {
            match conn.read_message().await {
                Ok(Some(msg)) => {
                    count += 1;
                    trace!("Received message from peer: {:?}", msg);

                    // Dispatch the message to the handler
                    let update = self.dispatch_message(msg, conn, &is_in_allowed_fast);

                    // Process dispatch updates: send Cancel for removed slots
                    for slot in &update.cancelled_slots {
                        if let Err(e) = conn
                            .send_cancel(
                                &aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                                    slot.index,
                                    slot.begin,
                                    slot.length,
                                ),
                            )
                            .await
                        {
                            warn!(
                                "Failed to send Cancel for piece {} begin {}: {}",
                                slot.index, slot.begin, e
                            );
                        }
                    }

                    // Collect inbound PEX updates for the caller.
                    if let Some(ext) = &update.extension_update
                        && matches!(ext, ExtensionUpdate::PeerExchange { .. }) {
                            last_pex_update = Some(ext.clone());
                        }

                    // Reset inactive timer on any received message
                    self.inactive_timer = Instant::now();
                }
                Ok(None) => {
                    // No more messages available
                    break;
                }
                Err(e) => {
                    // Read error - return it to the caller
                    return Err(e);
                }
            }
        }

        Ok((count, last_pex_update))
    }
}
