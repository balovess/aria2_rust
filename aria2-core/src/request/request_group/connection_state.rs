use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use super::{ActivitySignal, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

/// Protocol-neutral connection counters shared by all status consumers.
///
/// The counters mirror the original aria2 formula: stream connections (HTTP,
/// FTP, SFTP, or web-seed requests) plus active BitTorrent peer connections.
pub(crate) struct ConnectionState {
    stream_connections: std::sync::atomic::AtomicU32,
    http1_connections: std::sync::atomic::AtomicU32,
    http2_sessions: std::sync::atomic::AtomicU32,
    http2_streams: std::sync::atomic::AtomicU32,
    bt_connections: std::sync::atomic::AtomicU32,
    http_version: std::sync::atomic::AtomicU8,
    activity_signal: std::sync::OnceLock<Arc<ActivitySignal>>,
}

impl ConnectionState {
    pub(crate) fn new() -> Self {
        Self {
            stream_connections: std::sync::atomic::AtomicU32::new(0),
            http1_connections: std::sync::atomic::AtomicU32::new(0),
            http2_sessions: std::sync::atomic::AtomicU32::new(0),
            http2_streams: std::sync::atomic::AtomicU32::new(0),
            bt_connections: std::sync::atomic::AtomicU32::new(0),
            http_version: std::sync::atomic::AtomicU8::new(0),
            activity_signal: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn total(&self) -> u32 {
        self.stream_connections
            .load(Ordering::Acquire)
            .saturating_add(self.bt_connections.load(Ordering::Acquire))
    }

    fn set_stream(&self, count: usize) {
        self.stream_connections
            .store(count.min(u32::MAX as usize) as u32, Ordering::Release);
        self.notify_activity();
    }

    fn set_http_counts(
        &self,
        http1_connections: usize,
        http2_sessions: usize,
        http2_streams: usize,
    ) {
        self.http1_connections.store(
            http1_connections.min(u32::MAX as usize) as u32,
            Ordering::Release,
        );
        self.http2_sessions.store(
            http2_sessions.min(u32::MAX as usize) as u32,
            Ordering::Release,
        );
        self.http2_streams.store(
            http2_streams.min(u32::MAX as usize) as u32,
            Ordering::Release,
        );
        self.notify_activity();
    }

    fn http_counts(&self) -> (u32, u32, u32) {
        (
            self.http1_connections.load(Ordering::Acquire),
            self.http2_sessions.load(Ordering::Acquire),
            self.http2_streams.load(Ordering::Acquire),
        )
    }

    fn observe_http_version(&self, version: u8) {
        let _ = self
            .http_version
            .try_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |current| match current {
                    0 => Some(version),
                    3 => None,
                    current if current == version => None,
                    _ => Some(3),
                },
            );
        self.notify_activity();
    }

    fn http_version(&self) -> Option<&'static str> {
        match self.http_version.load(Ordering::Acquire) {
            1 => Some("HTTP/1.x"),
            2 => Some("HTTP/2"),
            3 => Some("mixed"),
            _ => None,
        }
    }

    pub(crate) fn set_bt(&self, count: usize) {
        self.bt_connections
            .store(count.min(u32::MAX as usize) as u32, Ordering::Release);
        self.notify_activity();
    }

    pub(crate) fn attach_activity_signal(&self, signal: Arc<ActivitySignal>) {
        let _ = self.activity_signal.set(signal);
    }

    fn notify_activity(&self) {
        if let Some(signal) = self.activity_signal.get() {
            signal.notify();
        }
    }
}

impl RequestGroup {
    /// Return the number of currently active protocol connections.
    pub fn active_connection_count(&self) -> u32 {
        self.connection_state.total()
    }

    /// Publish the current stream-protocol connection count.
    pub fn set_stream_connection_count(&self, count: usize) {
        self.connection_state.set_stream(count);
    }

    pub(crate) fn set_http_connection_counts(
        &self,
        http1_connections: usize,
        http2_sessions: usize,
        http2_streams: usize,
    ) {
        self.connection_state
            .set_http_counts(http1_connections, http2_sessions, http2_streams);
    }

    pub(crate) fn http_connection_counts(&self) -> (u32, u32, u32) {
        self.connection_state.http_counts()
    }

    /// Record the HTTP protocol version observed from a successful response.
    /// Different versions seen during one task are reported as mixed.
    pub fn observe_http_version(&self, http2: bool) {
        self.connection_state
            .observe_http_version(if http2 { 2 } else { 1 });
    }

    /// Return the negotiated HTTP version observed so far for this task.
    pub fn negotiated_http_version(&self) -> Option<&'static str> {
        self.connection_state.http_version()
    }

    /// Publish the current BitTorrent peer connection count.
    #[cfg(any(feature = "bittorrent", test))]
    pub(crate) fn set_bt_connection_count(&self, count: usize) {
        self.connection_state.set_bt(count);
    }

    #[cfg(feature = "bittorrent")]
    pub(crate) fn connection_state(&self) -> Arc<ConnectionState> {
        Arc::clone(&self.connection_state)
    }
}

/// Clears a protocol connection count when an async protocol operation exits.
///
/// The guard keeps cleanup local to the operation that owns the sockets. This
/// prevents early-return paths from leaving a stale connection count in RPC
/// or terminal UI snapshots.
pub(crate) struct ActiveConnectionGuard {
    group: Arc<RwLock<RequestGroup>>,
}

impl ActiveConnectionGuard {
    pub(crate) fn new(group: Arc<RwLock<RequestGroup>>) -> Self {
        let group_guard = group.recover();
        group_guard.set_stream_connection_count(0);
        group_guard.set_http_connection_counts(0, 0, 0);
        drop(group_guard);
        Self { group }
    }

    pub(crate) fn set(&self, count: usize) {
        self.group.recover().set_stream_connection_count(count);
    }

    pub(crate) fn set_http_counts(
        &self,
        http1_connections: usize,
        http2_sessions: usize,
        http2_streams: usize,
    ) {
        self.group.recover().set_http_connection_counts(
            http1_connections,
            http2_sessions,
            http2_streams,
        );
    }
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.group.recover().set_stream_connection_count(0);
        self.group.recover().set_http_connection_counts(0, 0, 0);
    }
}

/// Clears BitTorrent peer counts when the BT command lifecycle exits.
#[cfg(feature = "bittorrent")]
pub(crate) struct BtConnectionGuard {
    group: Arc<RwLock<RequestGroup>>,
}

#[cfg(feature = "bittorrent")]
impl BtConnectionGuard {
    pub(crate) fn new(group: Arc<RwLock<RequestGroup>>) -> Self {
        group.recover().set_bt_connection_count(0);
        Self { group }
    }
}

#[cfg(feature = "bittorrent")]
impl Drop for BtConnectionGuard {
    fn drop(&mut self) {
        self.group.recover().set_bt_connection_count(0);
    }
}
