use super::*;

// ==================================================================
// BEP 0027 (Private Torrent) enforcement tests
// ==================================================================

#[test]
fn test_private_torrent_is_private_flag_set() {
    let cmd = create_private_test_command();
    assert!(
        cmd.is_private,
        "is_private must be true for a torrent with private=1 in the info dict"
    );
}

#[test]
fn test_non_private_torrent_is_private_flag_false() {
    let cmd = create_test_command();
    assert!(
        !cmd.is_private,
        "is_private must be false for a torrent without the private flag"
    );
}

#[test]
fn test_private_torrent_dht_engine_not_started() {
    let cmd = create_private_test_command();
    // DHT engine is never initialized for private torrents. Even though it
    // starts as None by default, asserting here documents the BEP 0027
    // invariant: the download loop will never start DHT for this command.
    assert!(
        cmd.dht_engine.is_none(),
        "DHT engine must not be started for private torrents (BEP 0027)"
    );
}

#[test]
fn test_private_torrent_lpd_manager_not_started() {
    let cmd = create_private_test_command();
    // LPD manager is never initialized for private torrents.
    assert!(
        cmd.lpd_manager.is_none(),
        "LPD manager must not be started for private torrents (BEP 0027)"
    );
}

#[test]
fn test_private_torrent_pex_known_peers_empty() {
    let cmd = create_private_test_command();
    // PEX known peers list is never populated for private torrents.
    assert!(
        cmd.pex_known_peers.is_empty(),
        "PEX known peers must be empty for private torrents (BEP 0027)"
    );
}

#[test]
fn test_private_torrent_build_pex_extended_message_returns_none() {
    use aria2_protocol::bittorrent::peer::connection::PeerAddr;

    let mut cmd = create_private_test_command();

    // Even if peers were somehow added, build_pex_extended_message must refuse to
    // build a PEX message for private torrents.
    cmd.set_pex_known_peers(vec![
        PeerAddr::new("10.0.0.1", 6881),
        PeerAddr::new("10.0.0.2", 6881),
    ]);

    let remote = PeerAddr::new("10.0.0.99", 6881);
    let result = cmd.build_pex_extended_message(&remote, 2);
    assert!(
        result.is_none(),
        "build_pex_extended_message must return None for private torrents (BEP 0027) \
         even when pex_known_peers is populated"
    );
}

#[test]
fn test_private_torrent_handle_incoming_pex_ignored() {
    use aria2_protocol::bittorrent::peer::connection::PeerAddr;

    let mut cmd = create_private_test_command();

    // Construct a minimal valid PEX message (empty added/dropped lists).
    // The bencode for an empty PEX dict is "de".
    let pex_data = b"de";
    let local_addr = PeerAddr::new("127.0.0.1", 6881);

    let result = cmd.handle_incoming_pex(pex_data, &local_addr);
    assert!(
        result.is_ok(),
        "handle_incoming_pex should return Ok (with empty lists) for private torrents, \
         not an error"
    );
    let (added, dropped) = result.unwrap();
    assert!(
        added.is_empty() && dropped.is_empty(),
        "handle_incoming_pex must return empty lists for private torrents (BEP 0027)"
    );
}

#[test]
fn test_non_private_torrent_build_pex_extended_message_can_proceed() {
    use aria2_protocol::bittorrent::peer::connection::PeerAddr;

    let mut cmd = create_test_command();
    assert!(!cmd.is_private);

    // For a non-private torrent with peers populated, build_pex_extended_message should
    // be allowed to build a PEX message (it returns Some when ready).
    cmd.set_pex_known_peers(vec![PeerAddr::new("10.0.0.1", 6881)]);

    let remote = PeerAddr::new("10.0.0.99", 6881);
    let result = cmd.build_pex_extended_message(&remote, 2);
    assert!(
        result.is_some(),
        "build_pex_extended_message should return Some for non-private torrents with known peers"
    );
}
