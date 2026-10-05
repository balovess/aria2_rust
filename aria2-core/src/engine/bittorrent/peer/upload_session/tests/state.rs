use super::*;

#[tokio::test]
async fn startup_availability_matches_fast_extension_capabilities() {
    let mut no_pieces = InMemoryPieceProvider::new(16, 2);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    let mut non_fast_transport = TestUploadTransport::default();
    state
        .send_piece_availability(&mut non_fast_transport, &no_pieces)
        .await
        .unwrap();
    assert!(
        non_fast_transport.sent.is_empty(),
        "without Fast Extension, do not send an empty bitfield"
    );

    let mut fast_transport = TestUploadTransport {
        supports_fast_extension: true,
        ..TestUploadTransport::default()
    };
    state
        .send_piece_availability(&mut fast_transport, &no_pieces)
        .await
        .unwrap();
    assert_eq!(fast_transport.sent, [BtMessage::HaveNone]);

    no_pieces.set_piece_data(0, vec![0x11; 16]);
    non_fast_transport.sent.clear();
    state
        .send_piece_availability(&mut non_fast_transport, &no_pieces)
        .await
        .unwrap();
    assert_eq!(
        non_fast_transport.sent,
        [BtMessage::Bitfield {
            data: vec![0b1000_0000]
        }]
    );

    no_pieces.set_piece_data(1, vec![0x22; 16]);
    fast_transport.sent.clear();
    state
        .send_piece_availability(&mut fast_transport, &no_pieces)
        .await
        .unwrap();
    assert_eq!(fast_transport.sent, [BtMessage::HaveAll]);
}

#[tokio::test]
async fn outstanding_upload_count_tracks_queued_piece_responses() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x5a; 32]);
    let request = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
        index: 0,
        begin: 4,
        length: 8,
    };
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    let mut transport = TestUploadTransport::default();

    state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: request.clone(),
            },
            &provider,
        )
        .await
        .unwrap();
    assert_eq!(state.outstanding_upload_count(), 1);

    state
        .handle_message(
            &mut transport,
            BtMessage::Cancel {
                request: request.clone(),
            },
            &provider,
        )
        .await
        .unwrap();
    assert_eq!(state.outstanding_upload_count(), 0);

    state
        .handle_message(&mut transport, BtMessage::Request { request }, &provider)
        .await
        .unwrap();
    assert_eq!(state.outstanding_upload_count(), 1);
    state
        .flush_pending_messages(&mut transport, &provider)
        .await
        .unwrap();
    assert_eq!(state.outstanding_upload_count(), 0);
}

#[tokio::test]
async fn rate_limited_upload_flush_yields_and_preserves_the_request() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x5a; 32]);
    let limiter = crate::rate_limiter::RateLimiter::new(
        &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
    );
    let config = BtSeedingConfig {
        global_limiter: Some(limiter.clone()),
        ..BtSeedingConfig::default()
    };
    let mut state = BtUploadState::new(&config);
    let mut transport = TestUploadTransport::default();
    state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: PieceBlockRequest::new(0, 0, 8),
            },
            &provider,
        )
        .await
        .unwrap();

    let uploaded = tokio::time::timeout(
        Duration::from_millis(200),
        state.flush_pending_messages(&mut transport, &provider),
    )
    .await
    .expect("rate-limited flush must return control to the peer actor")
    .unwrap();
    assert_eq!(uploaded, 0);
    assert_eq!(state.outstanding_upload_count(), 1);

    limiter.set_upload_rate(None);
    assert_eq!(
        state
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap(),
        8
    );
    assert_eq!(state.outstanding_upload_count(), 0);
}

#[tokio::test]
async fn full_upload_queue_rejects_fast_peer_instead_of_silently_dropping_request() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x5a; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    let mut transport = TestUploadTransport {
        supports_fast_extension: true,
        ..TestUploadTransport::default()
    };
    for _ in 0..MAX_PENDING_UPLOAD_MESSAGES {
        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: PieceBlockRequest::new(0, 0, 8),
                },
                &provider,
            )
            .await
            .unwrap();
    }

    state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: PieceBlockRequest::new(0, 8, 8),
            },
            &provider,
        )
        .await
        .unwrap();

    assert_eq!(
        state.outstanding_upload_count(),
        MAX_PENDING_UPLOAD_MESSAGES
    );
    assert!(matches!(
        transport.sent.as_slice(),
        [BtMessage::Reject {
            index: 0,
            offset: 8,
            length: 8
        }]
    ));
}

#[tokio::test]
async fn full_upload_queue_disconnects_non_fast_peer_instead_of_silently_dropping_request() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x5a; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    let mut transport = TestUploadTransport::default();
    for _ in 0..MAX_PENDING_UPLOAD_MESSAGES {
        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: PieceBlockRequest::new(0, 0, 8),
                },
                &provider,
            )
            .await
            .unwrap();
    }

    assert!(
        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: PieceBlockRequest::new(0, 8, 8),
                },
                &provider,
            )
            .await
            .is_err()
    );
    assert_eq!(
        state.outstanding_upload_count(),
        MAX_PENDING_UPLOAD_MESSAGES
    );
}

#[tokio::test]
async fn cancel_is_processed_after_rate_limited_flush_yields() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x5a; 32]);
    let limiter = crate::rate_limiter::RateLimiter::new(
        &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
    );
    let config = BtSeedingConfig {
        global_limiter: Some(limiter),
        ..BtSeedingConfig::default()
    };
    let mut state = BtUploadState::new(&config);
    let mut transport = TestUploadTransport::default();
    let request = PieceBlockRequest::new(0, 0, 8);
    state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: request.clone(),
            },
            &provider,
        )
        .await
        .unwrap();
    state
        .flush_pending_messages(&mut transport, &provider)
        .await
        .unwrap();

    state
        .handle_message(&mut transport, BtMessage::Cancel { request }, &provider)
        .await
        .unwrap();
    assert_eq!(state.outstanding_upload_count(), 0);
    assert!(transport.sent.is_empty());
}

#[test]
fn test_seeding_config_default() {
    let cfg = BtSeedingConfig::default();
    assert!(cfg.max_upload_bytes_per_sec.is_none());
    assert_eq!(cfg.max_peers_to_unchoke, 4);
    assert_eq!(cfg.optimistic_unchoke_interval_secs, 30);
}

#[tokio::test]
async fn upload_state_serves_a_verified_piece_while_download_is_active() {
    let mut provider = InMemoryPieceProvider::new(32, 2);
    provider.set_piece_data(0, vec![0x5a; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    let mut transport = TestUploadTransport::default();

    state
        .handle_message(&mut transport, BtMessage::Interested, &provider)
        .await
        .unwrap();
    let uploaded = state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                    index: 0,
                    begin: 4,
                    length: 8,
                },
            },
            &provider,
        )
        .await
        .unwrap();
    let uploaded = uploaded
        + state
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap();

    assert_eq!(uploaded, 8);
    assert!(matches!(transport.sent.first(), Some(BtMessage::Unchoke)));
    assert!(matches!(
        transport.sent.get(1),
        Some(BtMessage::Piece { index: 0, begin: 4, data }) if data.as_ref() == [0x5a; 8]
    ));
    assert_eq!(state.uploaded_bytes(), 8);
}

#[tokio::test]
async fn upload_state_policy_keeps_peer_choked_until_rotation_allows_it() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x3c; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    state.set_auto_unchoke(false);
    let mut transport = TestUploadTransport::default();

    state
        .handle_message(&mut transport, BtMessage::Interested, &provider)
        .await
        .unwrap();
    let uploaded = state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                    index: 0,
                    begin: 0,
                    length: 8,
                },
            },
            &provider,
        )
        .await
        .unwrap();

    assert_eq!(uploaded, 0);
    assert!(state.is_peer_choked());
    assert!(transport.sent.is_empty());
}

#[tokio::test]
async fn choked_peer_can_request_piece_granted_by_allowed_fast() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x7b; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    state.set_auto_unchoke(false);
    let mut transport = TestUploadTransport {
        am_allowed_fast: [0].into_iter().collect(),
        ..Default::default()
    };

    let uploaded = state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                    index: 0,
                    begin: 0,
                    length: 8,
                },
            },
            &provider,
        )
        .await
        .unwrap();
    let uploaded = uploaded
        + state
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap();

    assert_eq!(uploaded, 8);
    assert!(matches!(
        transport.sent.as_slice(),
        [BtMessage::Piece { index: 0, begin: 0, data }] if data.as_ref() == [0x7b; 8]
    ));
}

#[tokio::test]
async fn choked_fast_peer_receives_reject_for_unallowed_piece_request() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x7b; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    state.set_auto_unchoke(false);
    let mut transport = TestUploadTransport {
        supports_fast_extension: true,
        ..Default::default()
    };

    let uploaded = state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                    index: 0,
                    begin: 4,
                    length: 8,
                },
            },
            &provider,
        )
        .await
        .unwrap();
    state
        .flush_pending_messages(&mut transport, &provider)
        .await
        .unwrap();

    assert_eq!(uploaded, 0);
    assert!(matches!(
        transport.sent.as_slice(),
        [BtMessage::Reject {
            index: 0,
            offset: 4,
            length: 8
        }]
    ));
}

#[tokio::test]
async fn cancel_removes_queued_piece_and_queues_fast_reject() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x6d; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    let mut transport = TestUploadTransport {
        supports_fast_extension: true,
        ..Default::default()
    };
    let request = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
        index: 0,
        begin: 8,
        length: 8,
    };

    state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: request.clone(),
            },
            &provider,
        )
        .await
        .unwrap();
    state
        .handle_message(&mut transport, BtMessage::Cancel { request }, &provider)
        .await
        .unwrap();

    assert_eq!(transport.sent.len(), 0);
    assert!(state.has_pending_messages());
    state
        .flush_pending_messages(&mut transport, &provider)
        .await
        .unwrap();
    assert!(matches!(
        transport.sent.as_slice(),
        [BtMessage::Reject {
            index: 0,
            offset: 8,
            length: 8
        }]
    ));
}

#[tokio::test]
async fn cancel_does_not_reject_when_fast_extension_is_unavailable() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x6d; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    let mut transport = TestUploadTransport::default();
    let request = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
        index: 0,
        begin: 0,
        length: 8,
    };

    state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: request.clone(),
            },
            &provider,
        )
        .await
        .unwrap();
    state
        .handle_message(&mut transport, BtMessage::Cancel { request }, &provider)
        .await
        .unwrap();
    state
        .flush_pending_messages(&mut transport, &provider)
        .await
        .unwrap();

    assert!(transport.sent.is_empty());
}

#[tokio::test]
async fn cancel_only_removes_an_exact_queued_piece_request() {
    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x6d; 32]);
    let mut state = BtUploadState::new(&BtSeedingConfig::default());
    let mut transport = TestUploadTransport::default();
    let request = PieceBlockRequest::new(0, 0, 8);

    state
        .handle_message(
            &mut transport,
            BtMessage::Request {
                request: request.clone(),
            },
            &provider,
        )
        .await
        .unwrap();
    state
        .handle_message(
            &mut transport,
            BtMessage::Cancel {
                request: PieceBlockRequest::new(0, 8, 8),
            },
            &provider,
        )
        .await
        .unwrap();
    state
        .flush_pending_messages(&mut transport, &provider)
        .await
        .unwrap();

    assert!(matches!(
        transport.sent.as_slice(),
        [BtMessage::Piece {
            index: 0,
            begin: 0,
            data
        }] if data.as_ref() == [0x6d; 8]
    ));
}
