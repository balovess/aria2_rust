use super::*;

#[tokio::test]
async fn cli_rebalances_unchoke_when_interested_peer_disconnects_during_idle_wait() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let log_path = output_dir.path().join("aria2.log");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = torrent(&placeholder_tracker.announce_url());
    let metadata = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent parses");
    let source = PieceSource::start(metadata.info_hash.bytes).await;
    drop(placeholder_tracker);
    let tracker = MockTrackerServer::start(source.addr.port()).await;
    let torrent = torrent(&tracker.announce_url());
    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--log={}", log_path.display()),
        "--log-level=debug".to_owned(),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-max-upload-slots=4".to_owned(),
        "--seed-time=0".to_owned(),
        "--seed-ratio=0".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let encoded = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(&client, 1, "aria2.addTorrent", json!([encoded, [], {}]))
        .as_str()
        .expect("addTorrent returns GID")
        .to_owned();

    let first_piece_result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                &client,
                2,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            if status["status"] == "active" && status["completedLength"] == "16" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if first_piece_result.is_err() {
        let status = rpc(
            &client,
            20,
            "aria2.tellStatus",
            json!([
                gid,
                ["status", "completedLength", "totalLength", "errorMessage"]
            ]),
        );
        let peers = rpc(&client, 21, "aria2.getPeers", json!([gid]));
        panic!(
            "first piece stalled: status={status}; peers={peers}; tracker_queries={:?}; source_addr={}; source_connections={}; valid_handshakes={}; requests={}; source_handshake={:02x?}; expected_info_hash={:02x?}",
            tracker.captured_queries().await,
            source.addr,
            source.accepted_connections.load(Ordering::SeqCst),
            source.valid_handshakes.load(Ordering::SeqCst),
            source.requests.load(Ordering::SeqCst),
            source
                .observed_handshake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            metadata.info_hash.bytes,
        );
    }

    let source_peer_id = char::from(0x53).to_string().repeat(20);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let peers = rpc(&client, 22, "aria2.getPeers", json!([gid]));
            if peers
                .as_array()
                .is_some_and(|peers| peers.iter().all(|peer| peer["peerId"] != source_peer_id))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the piece source should disconnect and leave the idle swarm before slot setup");

    let peer_ids = (0x61..=0x65)
        .map(|peer_id_byte| char::from(peer_id_byte).to_string().repeat(20))
        .collect::<Vec<_>>();
    let (wire_events_tx, mut wire_events_rx) = mpsc::unbounded_channel();
    let mut peers = Vec::with_capacity(peer_ids.len());
    for (peer_index, peer_id_byte) in (0x61..=0x65).enumerate() {
        peers.push(
            connect_interested_leecher(
                listen_port,
                metadata.info_hash.bytes,
                peer_id_byte,
                peer_index,
                wire_events_tx.clone(),
            )
            .await,
        );
    }
    drop(wire_events_tx);

    let (initially_choked, unchoke_generations) = tokio::time::timeout(
        Duration::from_secs(5),
        async {
            let mut previous_state = None;
            let mut stable_since = None;
            loop {
                let details = rpc(&client, 4, "aria2.getPeerDetails", json!([gid]));
                let snapshots = rpc(&client, 3, "aria2.getPeers", json!([gid]));
                let snapshots = snapshots.as_array().expect("getPeers returns an array");
                let wire_am_choking = peers
                    .iter()
                    .map(ConnectedLeecher::am_choking)
                    .collect::<Vec<_>>();
                let generations = peers
                    .iter()
                    .map(ConnectedLeecher::unchoke_generation)
                    .collect::<Vec<_>>();
                let all_present = snapshots.len() == peer_ids.len()
                    && peer_ids
                        .iter()
                        .all(|peer_id| snapshots.iter().any(|peer| peer["peerId"] == *peer_id));
                let all_interested = peer_ids.iter().all(|peer_id| {
                    details.as_array().is_some_and(|details| {
                        details.iter().any(|peer| {
                            peer["peerId"] == *peer_id && peer["flags"]["peerInterested"] == true
                        })
                    })
                });
                let rpc_matches_wire = peer_ids.iter().enumerate().all(|(index, peer_id)| {
                    snapshots.iter().any(|peer| {
                        peer["peerId"] == *peer_id
                            && peer["amChoking"]
                                == if wire_am_choking[index] {
                                    "true"
                                } else {
                                    "false"
                                }
                    })
                });
                let unchoked_count = wire_am_choking
                    .iter()
                    .filter(|&&choking| !choking)
                    .count();
                let has_choked_and_unchoked = wire_am_choking
                    .iter()
                    .any(|&choking| choking)
                    && unchoked_count > 0;
                let state_is_ready = all_present
                    && all_interested
                    && rpc_matches_wire
                    && has_choked_and_unchoked;
                let current_state = (wire_am_choking.clone(), generations);

                if state_is_ready {
                    if previous_state.as_ref() == Some(&current_state) {
                        if stable_since.is_some_and(|since: tokio::time::Instant| {
                            since.elapsed() >= Duration::from_millis(150)
                        }) {
                            break current_state;
                        }
                    } else {
                        previous_state = Some(current_state);
                        stable_since = Some(tokio::time::Instant::now());
                    }
                } else {
                    previous_state = None;
                    stable_since = None;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        },
    )
    .await
    .unwrap_or_else(|_| {
        let details = rpc(&client, 30, "aria2.getPeerDetails", json!([gid]));
        let snapshots = rpc(&client, 31, "aria2.getPeers", json!([gid]));
        let wire_am_choking = peers
            .iter()
            .map(ConnectedLeecher::am_choking)
            .collect::<Vec<_>>();
        panic!(
            "five connected interested peers did not settle into a stable mixed choke state: peers={snapshots}; details={details}; wire_am_choking={wire_am_choking:?}; expected_peer_ids={peer_ids:?}"
        );
    });

    let disconnected_index = initially_choked
        .iter()
        .position(|&choking| !choking)
        .expect("one peer owns an upload slot");
    let disconnected_peer_id = peer_ids[disconnected_index].as_str();
    drop(peers[disconnected_index].writer.take());
    peers[disconnected_index].wait_for_remote_disconnect().await;

    let mut post_disconnect_unchokes = vec![false; peers.len()];
    let convergence = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            while let Ok(event) = wire_events_rx.try_recv() {
                if event.message.first() == Some(&1)
                    && initially_choked[event.peer_index]
                    && event.unchoke_generation > unchoke_generations[event.peer_index]
                {
                    post_disconnect_unchokes[event.peer_index] = true;
                }
            }

            let snapshots = rpc(&client, 32, "aria2.getPeers", json!([gid]));
            let snapshots = snapshots.as_array().expect("getPeers returns an array");
            let details = rpc(&client, 35, "aria2.getPeerDetails", json!([gid]));
            let wire_am_choking = peers
                .iter()
                .map(ConnectedLeecher::am_choking)
                .collect::<Vec<_>>();
            let current_generations = peers
                .iter()
                .map(ConnectedLeecher::unchoke_generation)
                .collect::<Vec<_>>();
            let all_remaining_present = snapshots.len() == peer_ids.len() - 1
                && peer_ids.iter().enumerate().all(|(index, peer_id)| {
                    index == disconnected_index
                        || snapshots.iter().any(|peer| peer["peerId"] == *peer_id)
                })
                && snapshots
                    .iter()
                    .all(|peer| peer["peerId"] != disconnected_peer_id);
            let rpc_matches_wire = peer_ids.iter().enumerate().all(|(index, peer_id)| {
                index == disconnected_index
                    || snapshots.iter().any(|peer| {
                        peer["peerId"] == *peer_id
                            && peer["amChoking"]
                                == if wire_am_choking[index] {
                                    "true"
                                } else {
                                    "false"
                                }
                    })
            });
            let all_remaining_interested = peer_ids.iter().enumerate().all(|(index, peer_id)| {
                index == disconnected_index
                    || details.as_array().is_some_and(|details| {
                        details.iter().any(|peer| {
                            peer["peerId"] == *peer_id && peer["flags"]["peerInterested"] == true
                        })
                    })
            });
            let promoted_index = (0..peer_ids.len()).find(|&index| {
                index != disconnected_index
                    && initially_choked[index]
                    && post_disconnect_unchokes[index]
                    && current_generations[index] > unchoke_generations[index]
                    && !wire_am_choking[index]
            });
            if all_remaining_present
                && all_remaining_interested
                && rpc_matches_wire
                && let Some(promoted_index) = promoted_index
            {
                break (promoted_index, snapshots.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let (promoted_index, _snapshots) = convergence.unwrap_or_else(|_| {
        let details = rpc(&client, 33, "aria2.getPeerDetails", json!([gid]));
        let snapshots = rpc(&client, 34, "aria2.getPeers", json!([gid]));
        let wire_am_choking = peers
            .iter()
            .map(ConnectedLeecher::am_choking)
            .collect::<Vec<_>>();
        let current_generations = peers
            .iter()
            .map(ConnectedLeecher::unchoke_generation)
            .collect::<Vec<_>>();
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let relevant_log = log
            .lines()
            .filter(|line| {
                line.contains("seeding peer")
                    || line.contains("Seeding loop")
                    || line.contains("choke")
                    || line.contains("Choke")
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "RPC/wire state did not converge after upload-slot release: peers={snapshots}; details={details}; wire_am_choking={wire_am_choking:?}; initially_choked={initially_choked:?}; initial_unchoke_generations={unchoke_generations:?}; current_unchoke_generations={current_generations:?}; post_disconnect_unchokes={post_disconnect_unchokes:?}; choker_log={relevant_log}"
        );
    });
    assert_ne!(promoted_index, disconnected_index);

    let status = rpc(
        &client,
        4,
        "aria2.tellStatus",
        json!([gid, ["status", "completedLength", "totalLength"]]),
    );
    assert_eq!(status["status"], "active");
    assert_eq!(status["completedLength"], "16");
}
