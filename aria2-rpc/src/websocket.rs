use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::sync::broadcast;

/// Configuration for WebSocket connection keepalive behavior.
///
/// Controls ping/pong interval and timeout thresholds for detecting
/// stale or unresponsive WebSocket connections.
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// Interval in seconds between sending Ping frames (default: 30)
    pub ping_interval_secs: u64,
    /// Timeout in seconds after which a missed Pong triggers connection close (default: 60)
    pub pong_timeout_secs: u64,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            ping_interval_secs: 30,
            pong_timeout_secs: 60,
        }
    }
}

impl WsConfig {
    /// Create a new WsConfig with custom values.
    pub fn new(ping_interval_secs: u64, pong_timeout_secs: u64) -> Self {
        Self {
            ping_interval_secs,
            pong_timeout_secs,
        }
    }

    /// Builder-style method to set custom ping interval.
    pub fn with_ping_interval(mut self, secs: u64) -> Self {
        self.ping_interval_secs = secs;
        self
    }

    /// Builder-style method to set custom pong timeout.
    pub fn with_pong_timeout(mut self, secs: u64) -> Self {
        self.pong_timeout_secs = secs;
        self
    }
}

/// Download event types matching C++ aria2 WebSocketSessionMan.
///
/// C++ aria2 defines exactly 6 notification events.
/// Non-standard events (BtCacheChanged, BtDownloadError, DownloadResume)
/// are intentionally excluded for client compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EventType {
    DownloadStart,
    DownloadPause,
    DownloadStop,
    DownloadComplete,
    DownloadError,
    BtDownloadComplete,
}

impl EventType {
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::DownloadStart => crate::constants::WS_EVENT_DOWNLOAD_START,
            Self::DownloadPause => crate::constants::WS_EVENT_DOWNLOAD_PAUSE,
            Self::DownloadStop => crate::constants::WS_EVENT_DOWNLOAD_STOP,
            Self::DownloadComplete => crate::constants::WS_EVENT_DOWNLOAD_COMPLETE,
            Self::DownloadError => crate::constants::WS_EVENT_DOWNLOAD_ERROR,
            Self::BtDownloadComplete => crate::constants::WS_EVENT_BT_DOWNLOAD_COMPLETE,
        }
    }

    pub fn from_method(method: &str) -> Option<Self> {
        match method {
            crate::constants::WS_EVENT_DOWNLOAD_START => Some(Self::DownloadStart),
            crate::constants::WS_EVENT_DOWNLOAD_PAUSE => Some(Self::DownloadPause),
            crate::constants::WS_EVENT_DOWNLOAD_STOP => Some(Self::DownloadStop),
            crate::constants::WS_EVENT_DOWNLOAD_COMPLETE => Some(Self::DownloadComplete),
            crate::constants::WS_EVENT_DOWNLOAD_ERROR => Some(Self::DownloadError),
            crate::constants::WS_EVENT_BT_DOWNLOAD_COMPLETE => Some(Self::BtDownloadComplete),
            _ => None,
        }
    }
}

/// JSON-RPC notification matching C++ aria2 WebSocketSessionMan::addNotification format.
///
/// C++ format: `{"jsonrpc":"2.0","method":"aria2.onDownloadStart","params":[{"gid":"hexgid"}]}`
///
/// The `params` array contains a single object with only the `gid` field,
/// matching the C++ implementation exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadEvent {
    #[serde(rename = "jsonrpc")]
    version: String,
    method: String,
    params: Vec<serde_json::Value>,
}

impl DownloadEvent {
    /// Create a new notification event with custom params.
    pub fn new(event_type: EventType, params: Vec<serde_json::Value>) -> Self {
        Self {
            version: "2.0".to_string(),
            method: event_type.method_name().to_string(),
            params,
        }
    }

    /// Create a notification matching C++ format: `params: [{"gid": gid}]`.
    fn from_gid(event_type: EventType, gid: impl Into<String>) -> Self {
        Self::new(event_type, vec![serde_json::json!({"gid": gid.into()})])
    }

    pub fn event_type(&self) -> Option<EventType> {
        EventType::from_method(&self.method)
    }
    pub fn method(&self) -> &str {
        &self.method
    }
    pub fn params(&self) -> &[serde_json::Value] {
        &self.params
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Extract the GID from this event's params.
    ///
    /// Returns the GID string if present in the first param object,
    /// or an empty string if not found.
    pub fn gid(&self) -> String {
        self.params
            .first()
            .and_then(|p| p.get("gid"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    /// Get the event type as a string suitable for deduplication keys.
    ///
    /// Returns the method name (e.g., "aria2.onDownloadComplete") or empty string.
    pub fn event_type_str(&self) -> String {
        self.method.clone()
    }

    // -- C++ compatible notification constructors (5 core + BT event) --

    pub fn download_start(gid: impl Into<String>) -> Self {
        Self::from_gid(EventType::DownloadStart, gid)
    }

    pub fn download_complete(gid: impl Into<String>) -> Self {
        Self::from_gid(EventType::DownloadComplete, gid)
    }

    pub fn download_error(gid: impl Into<String>) -> Self {
        Self::from_gid(EventType::DownloadError, gid)
    }

    pub fn download_pause(gid: impl Into<String>) -> Self {
        Self::from_gid(EventType::DownloadPause, gid)
    }

    pub fn download_stop(gid: impl Into<String>) -> Self {
        Self::from_gid(EventType::DownloadStop, gid)
    }

    pub fn bt_download_complete(gid: impl Into<String>) -> Self {
        Self::from_gid(EventType::BtDownloadComplete, gid)
    }
}

struct Subscriber {
    filter: Option<Vec<EventType>>,
    sender: broadcast::Sender<(EventType, DownloadEvent)>,
}

/// Receiver registration whose lifetime is tied to the broadcast receiver.
pub struct ScopedSubscription {
    id: String,
    receiver: broadcast::Receiver<(EventType, DownloadEvent)>,
    subscribers: Arc<RwLock<HashMap<String, Subscriber>>>,
}

impl ScopedSubscription {
    /// Receive the next event accepted by this subscription's filter.
    ///
    /// Filtering happens before events enter this subscription's broadcast
    /// buffer, so unrelated events do not consume its lag budget. Broadcast
    /// lag is reported as `RecvError::Lagged` so callers can decide whether to
    /// resynchronize.
    pub async fn recv(
        &mut self,
    ) -> Result<(EventType, DownloadEvent), broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

impl Drop for ScopedSubscription {
    fn drop(&mut self) {
        if let Ok(mut subscribers) = self.subscribers.write() {
            subscribers.remove(&self.id);
        }
    }
}

pub struct EventPublisher {
    capacity: usize,
    subscribers: Arc<RwLock<HashMap<String, Subscriber>>>,
}

impl EventPublisher {
    pub fn new(capacity: usize) -> Self {
        // Preserve tokio::broadcast's validation for zero-capacity channels at
        // construction time, even though each subscriber owns its channel.
        let (_tx, _) = broadcast::channel::<(EventType, DownloadEvent)>(capacity);
        Self {
            capacity,
            subscribers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn register(
        &self,
        id: String,
        filter: Option<Vec<EventType>>,
        sender: broadcast::Sender<(EventType, DownloadEvent)>,
    ) {
        let mut subscribers = self
            .subscribers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        subscribers.insert(id, Subscriber { filter, sender });
    }

    pub async fn subscribe(
        &self,
        sub_id: impl Into<String>,
        filter: Option<Vec<EventType>>,
    ) -> ScopedSubscription {
        let id = sub_id.into();
        let (sender, receiver) = broadcast::channel(self.capacity);
        self.register(id.clone(), filter, sender);
        ScopedSubscription {
            id,
            receiver,
            subscribers: Arc::clone(&self.subscribers),
        }
    }

    /// Subscribe a connection whose bookkeeping must end with its receiver.
    ///
    /// This is intentionally crate-private: public callers retain the
    /// established explicit `subscribe` / `unsubscribe` lifecycle.
    pub(crate) fn subscribe_scoped(
        &self,
        sub_id: impl Into<String>,
        filter: Option<Vec<EventType>>,
    ) -> ScopedSubscription {
        let id = sub_id.into();
        let (sender, receiver) = broadcast::channel(self.capacity);
        self.register(id.clone(), filter, sender);
        ScopedSubscription {
            id,
            receiver,
            subscribers: Arc::clone(&self.subscribers),
        }
    }

    pub async fn unsubscribe(&self, sub_id: &str) {
        let mut subscribers = self
            .subscribers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        subscribers.remove(sub_id);
    }

    pub async fn subscriber_count(&self) -> usize {
        self.subscribers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn publish(&self, event_type: EventType, event: DownloadEvent) -> Result<usize, String> {
        let subscribers = self
            .subscribers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if subscribers.is_empty() {
            return Err("No subscribers".to_string());
        }

        let mut delivered = 0;
        for subscriber in subscribers.values() {
            let accepts = subscriber
                .filter
                .as_ref()
                .is_none_or(|filter| filter.contains(&event_type));
            if accepts
                && subscriber
                    .sender
                    .send((event_type.clone(), event.clone()))
                    .is_ok()
            {
                delivered += 1;
            }
        }

        Ok(delivered)
    }

    pub fn publish_event(&self, event: DownloadEvent) -> Result<usize, String> {
        if let Some(et) = event.event_type() {
            self.publish(et, event)
        } else {
            Err("Unknown event type".to_string())
        }
    }
}

impl Default for EventPublisher {
    fn default() -> Self {
        Self::new(256)
    }
}

// =========================================================================
// Notification Batcher (G7: Deduplication + Batching)
// =========================================================================

use std::collections::VecDeque;

/// Batches and deduplicates download event notifications.
///
/// Accumulates notifications and flushes them either when the batch size
/// limit is reached or when a time-based interval elapses. Deduplicates
/// events that share the same GID + event type combination, keeping only
/// the latest version (which may have updated progress/speed data).
///
/// # Example
///
/// ```ignore
/// let mut batcher = NotificationBatcher::new()
///     .with_max_batch_size(20)
///     .with_flush_interval_ms(500);
///
/// batcher.push(event1);
/// batcher.push(event2); // same GID+type as event1 → deduped
///
/// if let Some(batch) = batcher.maybe_flush() {
///     for notification in batch {
///         send_to_client(notification);
///     }
/// }
/// ```
pub struct NotificationBatcher {
    /// Maximum number of notifications before auto-flush triggers
    max_batch_size: usize,
    /// Time-based flush interval in milliseconds
    flush_interval_ms: u64,
    /// Accumulated notifications waiting to be flushed
    pending: VecDeque<String>,
    /// Tracks the latest event per (GID:event_type) key for deduplication
    latest_per_gid: HashMap<String, (DownloadEvent, Instant)>,
    /// Timestamp of last flush operation
    last_flush: Instant,
    /// Total number of notifications sent via flush
    total_sent: u64,
    /// Total number of notifications deduplicated (replaced by newer version)
    total_deduped: u64,
}

impl NotificationBatcher {
    /// Create a new NotificationBatcher with default settings.
    ///
    /// Defaults: max_batch_size=20, flush_interval_ms=500
    pub fn new() -> Self {
        Self {
            max_batch_size: 20,
            flush_interval_ms: 500,
            pending: VecDeque::new(),
            latest_per_gid: HashMap::new(),
            last_flush: Instant::now(),
            total_sent: 0,
            total_deduped: 0,
        }
    }

    /// Set the maximum batch size before auto-flush triggers.
    pub fn with_max_batch_size(mut self, size: usize) -> Self {
        self.max_batch_size = size;
        self
    }

    /// Set the timer-based flush interval in milliseconds.
    pub fn with_flush_interval_ms(mut self, ms: u64) -> Self {
        self.flush_interval_ms = ms;
        self
    }

    /// Push a notification into the batcher.
    ///
    /// Performs deduplication: if an event with the same (GID, event_type)
    /// already exists, it is replaced with this newer version.
    /// Returns `true` if the push triggered an auto-flush (batch full).
    pub fn push(&mut self, notification: DownloadEvent) -> bool {
        let gid = notification.gid();

        // Build dedup key from GID + event type combination
        let key = format!("{}:{}", gid, notification.method());

        if let Some(existing) = self.latest_per_gid.get_mut(&key) {
            // Replace with newer version (updated progress/speed/etc.)
            *existing = (notification, Instant::now());
            self.total_deduped += 1;
            tracing::debug!("Deduped notification for {}: {}", gid, key);
        } else {
            // New event: store in both the dedup map and pending queue
            self.latest_per_gid
                .insert(key.clone(), (notification, Instant::now()));
            self.pending.push_back(key);
        }

        // Auto-flush if over batch size limit
        if self.pending.len() >= self.max_batch_size {
            self.flush_internal();
            true
        } else {
            false
        }
    }

    /// Timer-based flush — call this periodically from main loop.
    ///
    /// Returns `Some(batch)` if notifications were flushed, `None` otherwise.
    /// Flushes when:
    /// - The flush interval has elapsed since last flush, AND
    /// - There are pending notifications
    pub fn maybe_flush(&mut self) -> Option<Vec<DownloadEvent>> {
        let elapsed_ms = self.last_flush.elapsed().as_millis() as u64;
        if elapsed_ms >= self.flush_interval_ms && !self.pending.is_empty() {
            self.flush_internal()
        } else {
            None
        }
    }

    /// Internal flush: drain all pending notifications and reset state.
    fn flush_internal(&mut self) -> Option<Vec<DownloadEvent>> {
        if self.pending.is_empty() {
            return None;
        }

        let batch: Vec<DownloadEvent> = self
            .pending
            .drain(..)
            .filter_map(|key| self.latest_per_gid.remove(&key).map(|(event, _)| event))
            .collect();
        self.total_sent += batch.len() as u64;
        self.last_flush = Instant::now();
        Some(batch)
    }

    /// Get statistics: (total_sent, total_deduped).
    pub fn stats(&self) -> (u64, u64) {
        (self.total_sent, self.total_deduped)
    }

    /// Returns the number of pending notifications awaiting flush.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Default for NotificationBatcher {
    fn default() -> Self {
        Self::new()
    }
}

pub struct WsSession {
    id: String,
    rx: broadcast::Receiver<(EventType, DownloadEvent)>,
}

impl WsSession {
    pub fn new(id: impl Into<String>, rx: broadcast::Receiver<(EventType, DownloadEvent)>) -> Self {
        Self { id: id.into(), rx }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Receive the next event while preserving broadcast closure and lag
    /// errors for callers that need explicit recovery behavior.
    pub async fn recv_result(
        &mut self,
    ) -> Result<(EventType, DownloadEvent), broadcast::error::RecvError> {
        self.rx.recv().await
    }

    pub async fn recv(&mut self) -> Option<(EventType, DownloadEvent)> {
        self.recv_result().await.ok()
    }
}

#[cfg(test)]
mod tests;
