use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, info};

use super::super::handler::DhtQueryHandler;
use super::super::message::{DhtMessage, DhtMessageType};
use super::super::node::DhtNode;
use super::super::tracker::TransactionTracker;
use super::{DhtEngine, DhtEngineContext};

const INBOUND_QUEUE_CAPACITY: usize = 1024;
const INBOUND_WORKERS: usize = 4;
const INBOUND_WORKER_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

type InboundPacket = (Vec<u8>, SocketAddr);

impl DhtEngine {
    /// Spawn the sole UDP reader and its bounded inbound workers.
    pub(super) fn spawn_receive_loop(
        self: &Arc<Self>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let context = Arc::clone(&self.context);
        let socket = context.task_context.socket.clone();
        let tracker = Arc::clone(&context.task_context.tracker);
        let tracker_notify = tracker.change_notifier();
        let handler_self_id = context.task_context.self_id;

        let handle = tokio::spawn(async move {
            // Independent receivers keep packet processing concurrent without
            // serializing `recv().await` behind a shared receiver mutex.
            let worker_capacity = INBOUND_QUEUE_CAPACITY.div_ceil(INBOUND_WORKERS);
            let mut worker_txs: Vec<mpsc::Sender<InboundPacket>> =
                Vec::with_capacity(INBOUND_WORKERS);
            let mut workers = JoinSet::new();

            for _ in 0..INBOUND_WORKERS {
                let (worker_tx, mut worker_rx) = mpsc::channel(worker_capacity);
                worker_txs.push(worker_tx);
                let worker_context = Arc::clone(&context);
                let worker_tracker = Arc::clone(&tracker);
                let worker_handler = DhtQueryHandler::new(handler_self_id);
                workers.spawn(async move {
                    while let Some((data, from)) = worker_rx.recv().await {
                        worker_context
                            .process_inbound_message(&data, from, &worker_tracker, &worker_handler)
                            .await;
                    }
                });
            }

            info!("DHT receive loop started");
            let mut buf = [0u8; 4096];
            let mut next_worker = 0usize;

            loop {
                if *shutdown_rx.borrow() {
                    break;
                }

                let timeout_wait = async {
                    match tracker.next_timeout() {
                        Some(timeout) => tokio::time::sleep(timeout).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::pin!(timeout_wait);
                let transaction_changed = tracker_notify.notified();
                tokio::pin!(transaction_changed);
                transaction_changed.as_mut().enable();

                let timeout_elapsed = tokio::select! {
                    result = shutdown_rx.changed() => {
                        if result.is_ok() {
                            info!("DHT receive loop shutting down");
                        }
                        break;
                    }
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((len, from)) if len > 0 => {
                                if !dispatch_inbound_packet(
                                    (buf[..len].to_vec(), from),
                                    &worker_txs,
                                    &mut next_worker,
                                ) {
                                    debug!(
                                        "DHT inbound workers busy; dropping packet from {}",
                                        from
                                    );
                                }
                            }
                            Ok(_) => { /* empty packet, ignore */ }
                            Err(error)
                                if error.kind() == std::io::ErrorKind::ConnectionReset =>
                            {
                                // Windows reports ICMP Port Unreachable for an outbound UDP
                                // query as WSAECONNRESET on the next receive. This is a
                                // per-datagram result, not a failure of the DHT socket.
                                debug!(
                                    local_addr = %context.task_context.socket.local_addr(),
                                    %error,
                                    "Ignoring DHT UDP connection reset"
                                );
                            }
                            Err(error) => {
                                debug!("DHT recv error: {}", error);
                                break;
                            }
                        }
                        false
                    }
                    _ = &mut timeout_wait => true,
                    _ = &mut transaction_changed => {
                        continue;
                    }
                };

                if timeout_elapsed {
                    // Query owners account for failures exactly once; doing it
                    // here would race their local response waits.
                    let expired = tracker.handle_timeouts();
                    if expired > 0 {
                        debug!(count = expired, "DHT transactions expired");
                    }
                }
            }

            // Closing senders lets workers drain queued datagrams before the
            // fixed shutdown deadline aborts any worker still blocked.
            drop(worker_txs);
            drain_inbound_workers(workers).await;
            info!("DHT receive loop exited");
        });
        self.register_background_task(handle);
    }
}

impl DhtEngineContext {
    async fn process_inbound_message(
        &self,
        data: &[u8],
        from: SocketAddr,
        tracker: &TransactionTracker,
        handler: &DhtQueryHandler,
    ) {
        let Ok(message) = DhtMessage::decode(data) else {
            return;
        };

        match message.y {
            DhtMessageType::Response | DhtMessageType::Error => {
                tracker.handle_response(message, from);
            }
            DhtMessageType::Query => {
                let (response, sender_to_promote) = {
                    let routing_table = self.task_context.routing_table.read().await;
                    let tokens = self
                        .token_tracker
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let result = handler.handle_query(
                        &message,
                        from,
                        &routing_table,
                        &tokens,
                        &self.peer_storage,
                        Some(&self.item_store),
                    );
                    (result.response, result.sender_to_promote)
                };

                if let Some(response) = response {
                    let encoded = response.encode();
                    if let Err(error) = self.task_context.socket.send_to(from, &encoded).await {
                        debug!(to = %from, "Failed to send DHT response: {}", error);
                    }
                }

                if let Some(sender_id) = sender_to_promote {
                    let mut routing_table = self.task_context.routing_table.write().await;
                    routing_table.mark_good(&sender_id);
                    routing_table.insert(DhtNode::new(sender_id, from));
                }
            }
        }
    }
}

fn dispatch_inbound_packet(
    mut packet: InboundPacket,
    worker_txs: &[mpsc::Sender<InboundPacket>],
    next_worker: &mut usize,
) -> bool {
    let worker_count = worker_txs.len();
    if worker_count == 0 {
        return false;
    }

    for offset in 0..worker_count {
        let worker_index = (*next_worker + offset) % worker_count;
        match worker_txs[worker_index].try_send(packet) {
            Ok(()) => {
                *next_worker = (worker_index + 1) % worker_count;
                return true;
            }
            Err(error) => packet = error.into_inner(),
        }
    }
    false
}

async fn drain_inbound_workers(mut workers: JoinSet<()>) {
    let wait_for_workers = async { while workers.join_next().await.is_some() {} };
    if tokio::time::timeout(INBOUND_WORKER_DRAIN_TIMEOUT, wait_for_workers)
        .await
        .is_err()
    {
        workers.abort_all();
        while workers.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::bittorrent::bencode::codec::BencodeValue;
    use crate::bittorrent::dht::engine::DhtEngineConfig;
    use crate::bittorrent::dht::message::DhtMessageBuilder;
    use crate::bittorrent::dht::tracker::QueryType;

    fn packet(id: u8) -> InboundPacket {
        (vec![id], "127.0.0.1:6881".parse().unwrap())
    }

    #[tokio::test]
    async fn malformed_response_does_not_consume_pending_transaction() {
        let engine = DhtEngine::start(DhtEngineConfig::local())
            .await
            .expect("local DHT engine should start");
        let tracker = Arc::clone(&engine.context.task_context.tracker);
        let from: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let (transaction_id, response_wait) =
            tracker.allocate_wait(QueryType::Ping, from, Duration::from_secs(30));
        let mut invalid_type =
            DhtMessageBuilder::ping_response(&transaction_id.to_be_bytes(), &[0x22; 20]).encode();
        let response_type = b"1:y1:r";
        let type_offset = invalid_type
            .windows(response_type.len())
            .rposition(|window| window == response_type)
            .expect("encoded response should contain its y field");
        invalid_type.splice(
            type_offset..type_offset + response_type.len(),
            b"1:y2:rx".iter().copied(),
        );
        let missing_result = BencodeValue::Dict(std::collections::BTreeMap::from([
            (
                b"t".to_vec(),
                BencodeValue::Bytes(transaction_id.to_be_bytes().to_vec()),
            ),
            (b"y".to_vec(), BencodeValue::Bytes(b"r".to_vec())),
        ]))
        .encode();

        for malformed in [invalid_type, missing_result] {
            engine
                .context
                .process_inbound_message(
                    &malformed,
                    from,
                    tracker.as_ref(),
                    &DhtQueryHandler::new(engine.context.task_context.self_id),
                )
                .await;
        }

        assert_eq!(tracker.pending_count(), 1);
        drop(response_wait);
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn protocol_error_reaches_waiter_as_failure() {
        let engine = DhtEngine::start(DhtEngineConfig::local())
            .await
            .expect("local DHT engine should start");
        let tracker = Arc::clone(&engine.context.task_context.tracker);
        let from: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let (transaction_id, response_wait) =
            tracker.allocate_wait(QueryType::Ping, from, Duration::from_secs(30));
        let error =
            DhtMessageBuilder::error_response(&transaction_id.to_be_bytes(), 203, "Protocol Error")
                .encode();

        engine
            .context
            .process_inbound_message(
                &error,
                from,
                tracker.as_ref(),
                &DhtQueryHandler::new(engine.context.task_context.self_id),
            )
            .await;

        assert_eq!(tracker.pending_count(), 0);
        assert!(response_wait.wait().await.is_none());
        engine.shutdown_async().await;
    }

    #[test]
    fn dispatch_skips_full_workers_and_rotates_start() {
        let (first_tx, mut first_rx) = mpsc::channel(1);
        first_tx.try_send(packet(1)).unwrap();
        let (second_tx, mut second_rx) = mpsc::channel(1);
        let worker_txs = [first_tx, second_tx];
        let mut next_worker = 0;

        assert!(dispatch_inbound_packet(
            packet(2),
            &worker_txs,
            &mut next_worker
        ));
        assert_eq!(next_worker, 0);
        assert_eq!(first_rx.try_recv().unwrap().0, vec![1]);
        assert_eq!(second_rx.try_recv().unwrap().0, vec![2]);

        assert!(dispatch_inbound_packet(
            packet(3),
            &worker_txs,
            &mut next_worker
        ));
        assert_eq!(next_worker, 1);
        assert_eq!(first_rx.try_recv().unwrap().0, vec![3]);
    }

    #[test]
    fn dispatch_drops_packet_when_all_worker_queues_are_full() {
        let (first_tx, mut first_rx) = mpsc::channel(1);
        first_tx.try_send(packet(1)).unwrap();
        let (second_tx, mut second_rx) = mpsc::channel(1);
        second_tx.try_send(packet(2)).unwrap();
        let worker_txs = [first_tx, second_tx];
        let mut next_worker = 0;

        assert!(!dispatch_inbound_packet(
            packet(3),
            &worker_txs,
            &mut next_worker
        ));
        assert_eq!(first_rx.try_recv().unwrap().0, vec![1]);
        assert_eq!(second_rx.try_recv().unwrap().0, vec![2]);
    }

    #[test]
    fn dispatch_skips_closed_worker() {
        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_rx);
        let (live_tx, mut live_rx) = mpsc::channel(1);
        let worker_txs = [closed_tx, live_tx];
        let mut next_worker = 0;

        assert!(dispatch_inbound_packet(
            packet(4),
            &worker_txs,
            &mut next_worker
        ));
        assert_eq!(live_rx.try_recv().unwrap().0, vec![4]);
    }

    #[tokio::test]
    async fn shutdown_drains_queued_packets_before_workers_exit() {
        let (sender, mut receiver) = mpsc::channel(2);
        let processed = Arc::new(AtomicUsize::new(0));
        let worker_processed = Arc::clone(&processed);
        let mut workers = JoinSet::new();
        workers.spawn(async move {
            while receiver.recv().await.is_some() {
                worker_processed.fetch_add(1, Ordering::Relaxed);
            }
        });

        sender.send(packet(1)).await.unwrap();
        sender.send(packet(2)).await.unwrap();
        drop(sender);
        drain_inbound_workers(workers).await;

        assert_eq!(processed.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn shutdown_aborts_workers_that_exceed_drain_deadline() {
        struct DropSignal(Arc<AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let worker_started = Arc::clone(&started);
        let worker_dropped = Arc::clone(&dropped);
        let mut workers = JoinSet::new();
        workers.spawn(async move {
            let _drop_signal = DropSignal(worker_dropped);
            worker_started.notify_one();
            std::future::pending::<()>().await;
        });
        started.notified().await;

        drain_inbound_workers(workers).await;

        assert!(dropped.load(Ordering::Acquire));
    }
}
