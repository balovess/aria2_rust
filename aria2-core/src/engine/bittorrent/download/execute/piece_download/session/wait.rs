use crate::engine::bittorrent::peer::connection::BtPeerConn;

use super::PieceLoopAction;

pub(super) enum NoPeerWaitEvent {
    Peer(super::super::peer_events::PeerWaitEvent),
    PeerDial(Option<std::result::Result<Vec<BtPeerConn>, tokio::task::JoinError>>),
    WebSeed(WebSeedTaskCompletion),
    UriChanged,
}

type WebSeedTaskCompletion = Option<
    std::result::Result<(u32, std::result::Result<Vec<u8>, String>), tokio::task::JoinError>,
>;

pub(super) enum PieceDownloadWait {
    Completed(Vec<(usize, PieceLoopAction)>),
    Incoming(Option<Box<crate::engine::bittorrent::peer::listener::IncomingPeer>>),
    PeerDial(Option<std::result::Result<Vec<BtPeerConn>, tokio::task::JoinError>>),
    StopTimeout,
}

pub(super) async fn wait_for_incoming_peer(
    receiver: Option<crate::engine::bittorrent::peer::listener::IncomingPeerReceiver>,
) -> Option<crate::engine::bittorrent::peer::listener::IncomingPeer> {
    match receiver {
        Some(receiver) => receiver.lock().await.recv().await,
        None => std::future::pending().await,
    }
}
