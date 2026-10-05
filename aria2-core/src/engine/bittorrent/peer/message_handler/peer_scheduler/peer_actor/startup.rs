//! Performs the one-time wire setup after a peer connection is handed to its actor.
use std::sync::Arc;

use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::upload_session::PieceDataProvider;
use crate::error::Result;

pub(super) async fn initialize_peer_actor(
    connection: &mut BtPeerConn,
    dht_engine: Option<&Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    upload_provider: Option<&dyn PieceDataProvider>,
) -> Result<(Option<(String, Option<u16>)>, bool)> {
    let Some(startup) = connection.actor_startup.take() else {
        return Ok((None, false));
    };

    let extension_handshake_info = Some((startup.peer_agent.clone(), startup.listen_port));
    let mut availability_sent = false;
    if connection.remote_supports_extended_messaging() {
        let metadata_size = connection
            .local_metadata
            .as_ref()
            .and_then(|metadata| u32::try_from(metadata.len()).ok())
            .filter(|size| *size > 0);
        connection
            .send_extension_handshake_with_metadata(
                &startup.peer_agent,
                startup.listen_port,
                metadata_size,
            )
            .await?;
    }
    if let Some(provider) = upload_provider {
        connection.announce_upload_availability(provider).await?;
        availability_sent = true;
    }
    if connection.remote_supports_dht()
        && let Some(engine) = dht_engine
    {
        connection.send_port(engine.local_addr().port()).await?;
    }
    if connection.remote_supports_fast_extension() {
        for piece_index in startup.allowed_fast {
            connection
                .send_bt_message(
                    &aria2_protocol::bittorrent::message::types::BtMessage::AllowedFast {
                        index: piece_index,
                    },
                )
                .await?;
            connection.add_am_allowed_fast(piece_index);
        }
    }
    Ok((extension_handshake_info, availability_sent))
}
