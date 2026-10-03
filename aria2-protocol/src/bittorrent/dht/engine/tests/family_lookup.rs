use super::super::{DhtEngine, DhtEngineConfig};
use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;

async fn assert_lookup_uses_only_socket_family(use_ipv6: bool, use_find_node: bool) {
    const INFO_HASH: [u8; 20] = [0x45; 20];
    let ipv4_node = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("IPv4 candidate should bind");
    let ipv4_node_addr = ipv4_node.local_addr().unwrap();
    let ipv6_node = UdpSocket::bind("[::1]:0")
        .await
        .expect("IPv6 candidate should bind");
    let ipv6_node_addr = ipv6_node.local_addr().unwrap();
    let IpAddr::V4(ipv4_node_ip) = ipv4_node_addr.ip() else {
        unreachable!("fixture is bound to IPv4 loopback")
    };
    let IpAddr::V6(ipv6_node_ip) = ipv6_node_addr.ip() else {
        unreachable!("fixture is bound to IPv6 loopback")
    };
    let bind_addr = if use_ipv6 { "[::1]:0" } else { "127.0.0.1:0" };
    let responder = UdpSocket::bind(bind_addr)
        .await
        .expect("loopback DHT responder should bind");
    let responder_addr = responder.local_addr().unwrap();
    let responder_task = tokio::spawn(async move {
        let mut packet = [0u8; 4096];
        loop {
            let (length, source) =
                tokio::time::timeout(Duration::from_secs(2), responder.recv_from(&mut packet))
                    .await
                    .expect("engine should issue DHT queries")
                    .expect("receive DHT query");
            let query = DhtMessage::decode(&packet[..length]).expect("valid DHT query");
            match query.q.as_ref().map(|method| method.0.as_str()) {
                Some("ping") => {
                    let response = DhtMessageBuilder::ping_response(&query.t, &[0x30; 20]);
                    responder.send_to(&response.encode(), source).await.unwrap();
                }
                Some("get_peers") | Some("find_node") => {
                    let mut nodes = vec![0x31; 20];
                    nodes.extend_from_slice(&ipv4_node_ip.octets());
                    nodes.extend_from_slice(&ipv4_node_addr.port().to_be_bytes());
                    let mut nodes6 = vec![0x32; 20];
                    nodes6.extend_from_slice(&ipv6_node_ip.octets());
                    nodes6.extend_from_slice(&ipv6_node_addr.port().to_be_bytes());

                    let mut result = BTreeMap::new();
                    result.insert(b"id".to_vec(), BencodeValue::Bytes([0x30; 20].to_vec()));
                    result.insert(b"token".to_vec(), BencodeValue::Bytes(b"token".to_vec()));
                    result.insert(b"nodes".to_vec(), BencodeValue::Bytes(nodes));
                    result.insert(b"nodes6".to_vec(), BencodeValue::Bytes(nodes6));
                    result.insert(
                        b"values".to_vec(),
                        BencodeValue::List(vec![
                            BencodeValue::Bytes(vec![127, 0, 0, 1, 0x1A, 0xE1]),
                            BencodeValue::Bytes(
                                Ipv6Addr::LOCALHOST
                                    .octets()
                                    .into_iter()
                                    .chain(6882u16.to_be_bytes())
                                    .collect(),
                            ),
                        ]),
                    );
                    let response = DhtMessage::new_response(query.t, BencodeValue::Dict(result));
                    responder.send_to(&response.encode(), source).await.unwrap();
                    break;
                }
                method => panic!("unexpected DHT query: {method:?}"),
            }
        }
    });

    let local_ip = if use_ipv6 {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    let engine = DhtEngine::start(DhtEngineConfig {
        listen_addr: Some(local_ip),
        query_timeout: Duration::from_millis(30),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("DHT engine should start");
    engine.add_node(responder_addr).await;

    if use_find_node {
        let result = crate::bittorrent::dht::lookup::iterative_find_node(
            &INFO_HASH,
            &engine.context.task_context.self_id,
            &engine.context.task_context.routing_table,
            &engine.context.task_context.socket,
            &engine.context.task_context.tracker,
            Duration::from_millis(30),
        )
        .await;
        assert!(
            result
                .closest_nodes
                .iter()
                .all(|node| node.addr().is_ipv6() == use_ipv6),
            "find_node must not admit nodes from the other address family: {:?}",
            result.closest_nodes
        );
    } else {
        let result = engine
            .find_peers(&INFO_HASH)
            .await
            .expect("peer lookup should complete");
        let expected_peer: SocketAddr = if use_ipv6 {
            "[::1]:6882".parse().unwrap()
        } else {
            "127.0.0.1:6881".parse().unwrap()
        };
        assert_eq!(result.peers, vec![expected_peer]);
    }
    assert_eq!(engine.stats().await.total_nodes, 2);

    engine.shutdown_async().await;
    responder_task.await.expect("DHT responder should finish");
}

#[tokio::test]
async fn ipv4_lookup_ignores_ipv6_nodes_and_peers_from_dual_stack_reply() {
    assert_lookup_uses_only_socket_family(false, false).await;
}

#[tokio::test]
async fn ipv6_lookup_ignores_ipv4_nodes_and_peers_from_dual_stack_reply() {
    assert_lookup_uses_only_socket_family(true, false).await;
}

#[tokio::test]
async fn ipv4_find_node_ignores_ipv6_nodes_from_dual_stack_reply() {
    assert_lookup_uses_only_socket_family(false, true).await;
}

#[tokio::test]
async fn ipv6_find_node_ignores_ipv4_nodes_from_dual_stack_reply() {
    assert_lookup_uses_only_socket_family(true, true).await;
}
