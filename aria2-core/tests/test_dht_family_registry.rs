#![cfg(feature = "bittorrent")]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use aria2_core::engine::bt_registry::BtRegistry;
use aria2_protocol::bittorrent::dht::engine::{DhtEngine, DhtEngineConfig};
use aria2_protocol::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
use aria2_protocol::bittorrent::dht::node::DhtNode;
use aria2_protocol::bittorrent::dht::persistence::DhtPersistence;
use tokio::net::UdpSocket;

async fn spawn_ping_responder(
    bind_addr: SocketAddr,
    node_id: [u8; 20],
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let socket = UdpSocket::bind(bind_addr)
        .await
        .expect("family-specific DHT responder should bind");
    let addr = socket.local_addr().expect("responder local address");
    let task = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (len, from) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf))
            .await
            .expect("DHT engine should ping the configured node")
            .expect("responder should receive a datagram");
        let query = DhtMessage::decode(&buf[..len]).expect("engine should send valid KRPC");
        assert_eq!(
            query.q.as_ref().map(|method| method.0.as_str()),
            Some("ping")
        );
        let response = DhtMessageBuilder::ping_response(&query.t, &node_id)
            .encode()
            .expect("ping response should encode");
        socket
            .send_to(&response, from)
            .await
            .expect("responder should return a valid ping response");
    });
    (addr, task)
}

async fn unused_udp_port(address: IpAddr) -> u16 {
    let socket = UdpSocket::bind(SocketAddr::new(address, 0))
        .await
        .expect("reserve an ephemeral UDP port");
    let port = socket.local_addr().expect("reserved UDP address").port();
    drop(socket);
    port
}

#[tokio::test]
async fn concurrent_registry_startup_creates_one_dht_engine_per_ip_family() {
    let registry = Arc::new(std::sync::RwLock::new(BtRegistry::new()));
    let ipv4_port = unused_udp_port(IpAddr::V4(Ipv4Addr::LOCALHOST)).await;
    let ipv6_port = unused_udp_port(IpAddr::V6(Ipv6Addr::LOCALHOST)).await;
    let ipv4_config = DhtEngineConfig {
        port: ipv4_port,
        listen_addr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ..DhtEngineConfig::local()
    };
    let ipv6_config = DhtEngineConfig {
        port: ipv6_port,
        listen_addr: Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ..DhtEngineConfig::local()
    };

    let (ipv4_first, ipv4_second, ipv6) = tokio::join!(
        BtRegistry::get_or_start_global_dht_engine(&registry, 1, ipv4_config.clone()),
        BtRegistry::get_or_start_global_dht_engine(&registry, 2, ipv4_config),
        BtRegistry::get_or_start_global_dht_engine(&registry, 3, ipv6_config),
    );
    let ipv4_first = ipv4_first.expect("first IPv4 DHT startup should succeed");
    let ipv4_second = ipv4_second.expect("concurrent IPv4 DHT startup should succeed");
    let ipv6 = ipv6.expect("IPv6 DHT startup should succeed");

    assert!(Arc::ptr_eq(&ipv4_first, &ipv4_second));
    assert_eq!(ipv4_first.local_addr().port(), ipv4_port);
    assert_ne!(ipv4_first.local_addr().ip(), ipv6.local_addr().ip());
    assert_eq!(ipv6.local_addr().port(), ipv6_port);
    {
        let registry_guard = registry.read().expect("BT registry should be readable");
        assert_eq!(registry_guard.get_dht_engines().len(), 2);
        assert!(
            registry_guard
                .get_global_dht_engine_for_peer(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    6881,
                ))
                .is_some_and(|engine| Arc::ptr_eq(&engine, &ipv4_first))
        );
        assert!(
            registry_guard
                .get_global_dht_engine_for_peer(SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::LOCALHOST),
                    6881,
                ))
                .is_some_and(|engine| Arc::ptr_eq(&engine, &ipv6))
        );
    }

    let engines = registry
        .write()
        .expect("BT registry should be writable")
        .take_global_dht_engines();
    for engine in engines {
        engine.shutdown_async().await;
    }
}

#[tokio::test]
async fn each_family_persists_and_restores_its_own_live_dht_snapshot() {
    let directory = tempfile::tempdir().expect("temporary DHT snapshot directory");
    let ipv4_path = directory.path().join("dht-v4.dat");
    let ipv6_path = directory.path().join("dht-v6.dat");
    let (ipv4_node_addr, ipv4_responder) =
        spawn_ping_responder("127.0.0.1:0".parse().unwrap(), [0x41; 20]).await;
    let (ipv6_node_addr, ipv6_responder) =
        spawn_ping_responder("[::1]:0".parse().unwrap(), [0x61; 20]).await;
    let stale_ipv6_socket = UdpSocket::bind("[::1]:0")
        .await
        .expect("temporary socket should reserve an IPv6 port");
    let stale_ipv6_node_addr = stale_ipv6_socket
        .local_addr()
        .expect("temporary IPv6 socket address");
    assert_ne!(stale_ipv6_node_addr, ipv6_node_addr);
    drop(stale_ipv6_socket);

    DhtPersistence::save_to_file_sync(
        &ipv4_path,
        &[0x31; 20],
        &[DhtNode::new(
            [0x32; 20],
            "127.0.0.2:6881".parse().expect("IPv4 stale node address"),
        )],
    )
    .expect("seed an older IPv4 snapshot");
    DhtPersistence::save_to_file_sync(
        &ipv6_path,
        &[0x51; 20],
        &[DhtNode::new([0x52; 20], stale_ipv6_node_addr)],
    )
    .expect("seed an older IPv6 snapshot");
    let ipv4_config = DhtEngineConfig {
        listen_addr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        dht_file_path: Some(ipv4_path.clone()),
        ..DhtEngineConfig::local()
    };
    let ipv6_config = DhtEngineConfig {
        listen_addr: Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        dht_file_path: Some(ipv6_path.clone()),
        ..DhtEngineConfig::local()
    };
    let registry = Arc::new(std::sync::RwLock::new(BtRegistry::new()));
    let ipv4 = BtRegistry::get_or_start_global_dht_engine(&registry, 41, ipv4_config.clone())
        .await
        .expect("IPv4 engine should start through the process registry");
    let ipv6 = BtRegistry::get_or_start_global_dht_engine(&registry, 42, ipv6_config.clone())
        .await
        .expect("IPv6 engine should start through the process registry");
    assert_eq!(ipv4.local_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(ipv6.local_addr().ip(), IpAddr::V6(Ipv6Addr::LOCALHOST));
    assert_eq!(ipv4.stats().await.good_nodes, 0);
    let initial_ipv6_stats = ipv6.stats().await;
    assert_eq!(initial_ipv6_stats.total_nodes, 1);
    assert_eq!(initial_ipv6_stats.good_nodes, 0);

    ipv4.save_state()
        .await
        .expect("IPv4 engine should replace stale data with its live snapshot");
    ipv6.save_state()
        .await
        .expect("IPv6 engine should replace stale data with its live snapshot");
    assert!(
        DhtPersistence::load_from_file_sync(&ipv4_path)
            .expect("IPv4 snapshot should be readable")
            .nodes
            .is_empty()
    );
    assert!(
        DhtPersistence::load_from_file_sync(&ipv6_path)
            .expect("IPv6 snapshot should be readable")
            .nodes
            .is_empty()
    );

    ipv4.add_node(ipv4_node_addr).await;
    ipv4_responder
        .await
        .expect("IPv4 DHT responder should finish");
    ipv6.add_node(ipv6_node_addr).await;
    ipv6_responder
        .await
        .expect("IPv6 DHT responder should finish");
    assert_eq!(ipv4.stats().await.good_nodes, 1);
    let live_ipv6_stats = ipv6.stats().await;
    assert_eq!(
        live_ipv6_stats.good_nodes, 1,
        "IPv6 live node was not validated after loading a saved node: {live_ipv6_stats:?}"
    );

    ipv4.save_state()
        .await
        .expect("IPv4 engine should save its validated live node");
    ipv6.save_state()
        .await
        .expect("IPv6 engine should save its validated live node");
    let ipv4_checkpoint = DhtPersistence::load_from_file_sync(&ipv4_path)
        .expect("IPv4 live snapshot should be readable");
    let ipv6_checkpoint = DhtPersistence::load_from_file_sync(&ipv6_path)
        .expect("IPv6 live snapshot should be readable");
    assert_eq!(ipv4_checkpoint.nodes.len(), 1);
    assert_eq!(ipv4_checkpoint.nodes[0].addr, ipv4_node_addr);
    assert_eq!(ipv6_checkpoint.nodes.len(), 1);
    assert_eq!(ipv6_checkpoint.nodes[0].addr, ipv6_node_addr);

    let engines = registry
        .write()
        .expect("BT registry should be writable")
        .take_global_dht_engines();
    assert_eq!(engines.len(), 2);
    for engine in engines {
        engine.shutdown_async().await;
    }
    let ipv4_snapshot =
        DhtPersistence::load_from_file_sync(&ipv4_path).expect("IPv4 snapshot should be readable");
    let ipv6_snapshot =
        DhtPersistence::load_from_file_sync(&ipv6_path).expect("IPv6 snapshot should be readable");
    assert_eq!(ipv4_snapshot.nodes.len(), 1);
    assert_eq!(ipv4_snapshot.nodes[0].addr, ipv4_node_addr);
    assert_eq!(ipv6_snapshot.nodes.len(), 1);
    assert_eq!(ipv6_snapshot.nodes[0].addr, ipv6_node_addr);

    let restored_ipv4 = DhtEngine::start(ipv4_config)
        .await
        .expect("IPv4 engine should restore its own snapshot");
    let restored_ipv6 = DhtEngine::start(ipv6_config)
        .await
        .expect("IPv6 engine should restore its own snapshot");
    let ipv4_stats = restored_ipv4.stats().await;
    let ipv6_stats = restored_ipv6.stats().await;
    assert_eq!(
        restored_ipv4.local_addr().ip(),
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    );
    assert_eq!(
        restored_ipv6.local_addr().ip(),
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    );
    assert_eq!(ipv4_stats.total_nodes, 1);
    assert_eq!(
        ipv4_stats.good_nodes, 0,
        "restored nodes must be revalidated"
    );
    assert_eq!(ipv6_stats.total_nodes, 1);
    assert_eq!(
        ipv6_stats.good_nodes, 0,
        "restored nodes must be revalidated"
    );

    restored_ipv4.shutdown_async().await;
    restored_ipv6.shutdown_async().await;
}
