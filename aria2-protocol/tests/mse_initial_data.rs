#![cfg(feature = "bittorrent")]

use aria2_protocol::bittorrent::extension::mse_crypto::{
    MseCryptoMethod, MseDerivedKeys, Rc4State, compute_vc, init_rc4,
};
use aria2_protocol::bittorrent::extension::mse_dh::{MseDhKeyExchange, VC_LENGTH};
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::peer::incoming::receive;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const INFO_HASH: [u8; 20] = [0x35; 20];
const REMOTE_PEER_ID: [u8; 20] = [0x53; 20];

type IncomingServer = JoinHandle<Result<Option<[u8; 20]>, String>>;

async fn begin_incoming_mse(ia: &[u8]) -> (TcpStream, IncomingServer, MseDerivedKeys, Rc4State) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let incoming = receive(stream, &[INFO_HASH]).await?;
        let connection = incoming.complete([0x54; 20], None, false).await?;
        Ok::<_, String>(connection.remote_peer_id().copied())
    });

    let mut client = TcpStream::connect(address).await.unwrap();
    let initiator_dh = MseDhKeyExchange::new();
    client
        .write_all(&initiator_dh.generate_public_key())
        .await
        .unwrap();
    let mut responder_public_key = [0u8; 96];
    client.read_exact(&mut responder_public_key).await.unwrap();

    let shared_secret = initiator_dh.compute_shared_secret(&responder_public_key);
    let keys = MseDerivedKeys::derive(&shared_secret, &INFO_HASH);
    let mut step3 = keys.req1.to_vec();
    step3.extend(
        keys.req2
            .iter()
            .zip(keys.req3.iter())
            .map(|(req2, req3)| req2 ^ req3),
    );

    let mut encrypted = Vec::with_capacity(VC_LENGTH + 4 + 2 + 2 + ia.len());
    encrypted.extend_from_slice(&[0u8; VC_LENGTH]);
    encrypted.extend_from_slice(&MseCryptoMethod::Rc4.as_u32().to_be_bytes());
    encrypted.extend_from_slice(&0u16.to_be_bytes());
    encrypted.extend_from_slice(&(ia.len() as u16).to_be_bytes());
    encrypted.extend_from_slice(ia);
    let mut send_cipher = init_rc4(&keys.key_a);
    send_cipher.process(&mut encrypted);
    step3.extend_from_slice(&encrypted);
    client.write_all(&step3).await.unwrap();

    (client, server, keys, send_cipher)
}

async fn read_incoming_mse_response(stream: &mut TcpStream, key_b: &[u8; 20]) {
    let mut marker_cipher = init_rc4(key_b);
    let vc_marker = compute_vc(&mut marker_cipher);
    let mut received = Vec::new();

    loop {
        let mut chunk = [0u8; 64];
        let length =
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut chunk))
                .await
                .expect("MSE response should arrive")
                .expect("read MSE response");
        assert_ne!(length, 0, "incoming peer closed before the MSE response");
        received.extend_from_slice(&chunk[..length]);

        let Some(vc_position) = received
            .windows(vc_marker.len())
            .position(|window| window == vc_marker)
        else {
            continue;
        };
        let header_end = vc_position + VC_LENGTH + 4 + 2;
        if received.len() < header_end {
            continue;
        }

        let mut header = received[vc_position..header_end].to_vec();
        init_rc4(key_b).process(&mut header);
        assert_eq!(&header[..VC_LENGTH], &[0u8; VC_LENGTH]);
        let pad_d_length =
            u16::from_be_bytes([header[VC_LENGTH + 4], header[VC_LENGTH + 5]]) as usize;
        if received.len() >= header_end + pad_d_length {
            return;
        }
    }
}

async fn completed_remote_peer_id(mut server: IncomingServer) -> Option<[u8; 20]> {
    match tokio::time::timeout(std::time::Duration::from_secs(2), &mut server).await {
        Ok(result) => result.unwrap().unwrap(),
        Err(_) => {
            server.abort();
            panic!("incoming MSE handshake failed to consume IA and complete the peer handshake");
        }
    }
}

#[tokio::test]
async fn incoming_mse_accepts_a_complete_bittorrent_handshake_in_ia() {
    let ia = Handshake::new(&INFO_HASH, &REMOTE_PEER_ID).to_bytes();
    assert_eq!(ia.len(), 68);

    let (_client, server, _keys, _send_cipher) = begin_incoming_mse(&ia).await;
    assert_eq!(completed_remote_peer_id(server).await, Some(REMOTE_PEER_ID));
}

#[tokio::test]
async fn incoming_mse_continues_the_handshake_after_a_partial_ia_prefix() {
    let handshake = Handshake::new(&INFO_HASH, &REMOTE_PEER_ID).to_bytes();
    let ia_length = 24;
    let (mut client, server, keys, mut send_cipher) =
        begin_incoming_mse(&handshake[..ia_length]).await;

    read_incoming_mse_response(&mut client, &keys.key_b).await;
    let mut remaining = handshake[ia_length..].to_vec();
    send_cipher.process(&mut remaining);
    client.write_all(&remaining).await.unwrap();

    assert_eq!(completed_remote_peer_id(server).await, Some(REMOTE_PEER_ID));
}

#[tokio::test]
async fn incoming_mse_rejects_ia_larger_than_the_bittorrent_handshake() {
    let ia = [0xA5; 69];
    let (_client, mut server, _keys, _send_cipher) = begin_incoming_mse(&ia).await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), &mut server)
        .await
        .expect("oversized IA should be rejected promptly")
        .unwrap();
    assert!(matches!(result, Err(error) if error.contains("IA length too large")));
}
