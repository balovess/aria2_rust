use super::*;

#[test]
fn local_metadata_response_serves_bep9_chunks_and_rejects_out_of_range_pieces() {
    let metadata = vec![0x5a; METADATA_PIECE_SIZE + 7];

    assert_eq!(
        local_metadata_response(Some(&metadata), 0),
        aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Data {
            piece: 0,
            total_size: metadata.len() as u32,
            data: vec![0x5a; METADATA_PIECE_SIZE],
        }
    );
    assert_eq!(
        local_metadata_response(Some(&metadata), 1),
        aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Data {
            piece: 1,
            total_size: metadata.len() as u32,
            data: vec![0x5a; 7],
        }
    );
    assert_eq!(
        local_metadata_response(Some(&metadata), 2),
        aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Reject { piece: 2 }
    );
    assert_eq!(
        local_metadata_response(None, 0),
        aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Reject { piece: 0 }
    );
}
#[test]
fn generation_end_updates_snapshot_when_bounded_peer_mailbox_is_full() {
    let (control, mut receiver) = PeerActorControl::channel(1);
    control
        .try_send(PeerCommand::HavePiece { piece_index: 4 })
        .unwrap();
    let generation = RequestGeneration::allocate();

    control.begin_generation(generation, 9).unwrap();
    control.end_generation(generation, 9).unwrap();

    assert!(matches!(
        receiver.commands.try_recv(),
        Ok(PeerCommand::HavePiece { piece_index: 4 })
    ));
    assert!(receiver.generation_updates.borrow().is_empty());
}

#[test]
fn generation_start_updates_snapshot_when_bounded_peer_mailbox_is_full() {
    let (control, mut receiver) = PeerActorControl::channel(1);
    control
        .try_send(PeerCommand::HavePiece { piece_index: 4 })
        .unwrap();
    let generation = RequestGeneration::allocate();

    control.begin_generation(generation, 9).unwrap();

    assert_eq!(
        receiver.generation_updates.borrow().get(&9),
        Some(&generation)
    );
    assert!(matches!(
        receiver.commands.try_recv(),
        Ok(PeerCommand::HavePiece { piece_index: 4 })
    ));
}

#[test]
fn generation_updates_do_not_accumulate_while_actor_is_not_polling() {
    let (control, receiver) = PeerActorControl::channel(1);
    let old_generation = RequestGeneration::allocate();
    let current_generation = RequestGeneration::allocate();
    let other_piece_generation = RequestGeneration::allocate();

    control.begin_generation(old_generation, 9).unwrap();
    control
        .begin_generation(other_piece_generation, 10)
        .unwrap();
    control.begin_generation(current_generation, 9).unwrap();
    control.end_generation(old_generation, 9).unwrap();

    let active = receiver.generation_updates.borrow();
    assert_eq!(active.get(&9), Some(&current_generation));
    assert_eq!(active.get(&10), Some(&other_piece_generation));
    assert_eq!(
        active.len(),
        2,
        "the snapshot keeps each piece's current generation but no history"
    );
}

#[tokio::test]
async fn desired_peer_state_updates_do_not_wait_for_bounded_mailbox_capacity() {
    let (control, mut receiver) = PeerActorControl::channel(1);
    control
        .try_send(PeerCommand::HavePiece { piece_index: 4 })
        .unwrap();

    assert!(control.set_upload_choked(false));
    control
        .set_wanted_pieces(Arc::from([0x80]))
        .expect("the actor should still receive coalesced state updates");
    assert!(control.set_upload_choked(true));
    control
        .set_wanted_pieces(Arc::from([0x40]))
        .expect("the latest state should replace the pending intermediate value");

    receiver.desired_state_updates.changed().await.unwrap();
    let desired = receiver.desired_state_updates.borrow_and_update();
    assert_eq!(desired.wanted_pieces.as_ref(), &[0x40]);
    assert!(desired.choke_upload);
    assert!(matches!(
        receiver.commands.try_recv(),
        Ok(PeerCommand::HavePiece { piece_index: 4 })
    ));
}

#[tokio::test]
async fn bounded_mailbox_capacity_release_wakes_waiter() {
    let (control, mut receiver) = PeerActorControl::channel(1);
    control
        .try_send(PeerCommand::HavePiece { piece_index: 1 })
        .unwrap();
    let mut capacity_updates = control.queue_capacity_updates();
    assert!(matches!(
        control.try_send(PeerCommand::HavePiece { piece_index: 2 }),
        Err(mpsc::error::TrySendError::Full(_))
    ));

    let command = receiver.commands.recv().await;
    assert!(matches!(
        command,
        Some(PeerCommand::HavePiece { piece_index: 1 })
    ));
    notify_queue_capacity(&receiver.capacity_updates);
    tokio::time::timeout(Duration::from_secs(1), capacity_updates.changed())
        .await
        .expect("dequeue should wake capacity waiters")
        .expect("capacity watch should remain open");
    assert!(
        control
            .try_send(PeerCommand::HavePiece { piece_index: 3 })
            .is_ok()
    );
}
