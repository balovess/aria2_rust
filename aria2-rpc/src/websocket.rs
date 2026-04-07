use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EventType {
    DownloadStart,
    DownloadPause,
    DownloadStop,
    DownloadComplete,
    DownloadError,
    BtDownloadComplete,
    BtDownloadError,
}

impl EventType {
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::DownloadStart => "aria2.onDownloadStart",
            Self::DownloadPause => "aria2.onDownloadPause",
            Self::DownloadStop => "aria2.onDownloadStop",
            Self::DownloadComplete => "aria2.onDownloadComplete",
            Self::DownloadError => "aria2.onDownloadError",
            Self::BtDownloadComplete => "aria2.onBtDownloadComplete",
            Self::BtDownloadError => "aria2.onBtDownloadError",
        }
    }

    pub fn from_method(method: &str) -> Option<Self> {
        match method {
            "aria2.onDownloadStart" => Some(Self::DownloadStart),
            "aria2.onDownloadPause" => Some(Self::DownloadPause),
            "aria2.onDownloadStop" => Some(Self::DownloadStop),
            "aria2.onDownloadComplete" => Some(Self::DownloadComplete),
            "aria2.onDownloadError" => Some(Self::DownloadError),
            "aria2.onBtDownloadComplete" => Some(Self::BtDownloadComplete),
            "aria2.onBtDownloadError" => Some(Self::BtDownloadError),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadEvent {
    #[serde(rename = "jsonrpc")]
    version: String,
    method: String,
    params: Vec<serde_json::Value>,
}

impl DownloadEvent {
    pub fn new(event_type: EventType, params: Vec<serde_json::Value>) -> Self {
        Self { version: "2.0".to_string(), method: event_type.method_name().to_string(), params }
    }

    pub fn event_type(&self) -> Option<EventType> { EventType::from_method(&self.method) }
    pub fn method(&self) -> &str { &self.method }
    pub fn params(&self) -> &[serde_json::Value] { &self.params }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn download_start(gid: impl Into<String>, files: Vec<serde_json::Value>) -> Self {
        Self::new(EventType::DownloadStart, vec![serde_json::json!({"gid": gid.into(), "files": files})])
    }

    pub fn download_complete(gid: impl Into<String>, files: Vec<serde_json::Value>) -> Self {
        Self::new(EventType::DownloadComplete, vec![serde_json::json!({"gid": gid.into(), "files": files})])
    }

    pub fn download_error(gid: impl Into<String>, error_code: i32, files: Vec<serde_json::Value>) -> Self {
        Self::new(EventType::DownloadError, vec![serde_json::json!({"gid": gid.into(), "errorCode": error_code, "files": files})])
    }

    pub fn download_pause(gid: impl Into<String>) -> Self {
        Self::new(EventType::DownloadPause, vec![serde_json::json!({"gid": gid.into()})])
    }

    pub fn download_stop(gid: impl Into<String>, files: Vec<serde_json::Value>) -> Self {
        Self::new(EventType::DownloadStop, vec![serde_json::json!({"gid": gid.into(), "files": files})])
    }

    pub fn bt_download_complete(gid: impl Into<String>, files: Vec<serde_json::Value>) -> Self {
        Self::new(EventType::BtDownloadComplete, vec![serde_json::json!({"gid": gid.into(), "files": files})])
    }

    pub fn bt_download_error(gid: impl Into<String>, error_code: i32, files: Vec<serde_json::Value>) -> Self {
        Self::new(EventType::BtDownloadError, vec![serde_json::json!({"gid": gid.into(), "errorCode": error_code, "files": files})])
    }
}

#[derive(Clone)]
struct Subscriber {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    filter: Option<Vec<EventType>>,
}

pub struct EventPublisher {
    tx: broadcast::Sender<(EventType, DownloadEvent)>,
    subscribers: Arc<RwLock<HashMap<String, Subscriber>>>,
}

impl EventPublisher {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx, subscribers: Arc::new(RwLock::new(HashMap::new())) }
    }

    pub async fn subscribe(&self, sub_id: impl Into<String>, filter: Option<Vec<EventType>>) -> broadcast::Receiver<(EventType, DownloadEvent)> {
        let mut subs = self.subscribers.write().await;
        let id = sub_id.into();
        subs.insert(id.clone(), Subscriber { id: id.clone(), filter });
        self.tx.subscribe()
    }

    pub async fn unsubscribe(&self, sub_id: &str) {
        let mut subs = self.subscribers.write().await;
        subs.remove(sub_id);
    }

    pub async fn subscriber_count(&self) -> usize {
        self.subscribers.read().await.len()
    }

    pub fn publish(&self, event_type: EventType, event: DownloadEvent) -> Result<usize, String> {
        match self.tx.send((event_type, event)) {
            Ok(_) => Ok(self.tx.receiver_count()),
            Err(e) => Err(format!("No subscribers: {}", e)),
        }
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
    fn default() -> Self { Self::new(256) }
}

pub struct WsSession {
    id: String,
    rx: broadcast::Receiver<(EventType, DownloadEvent)>,
}

impl WsSession {
    pub fn new(id: impl Into<String>, rx: broadcast::Receiver<(EventType, DownloadEvent)>) -> Self {
        Self { id: id.into(), rx }
    }

    pub fn id(&self) -> &str { &self.id }

    pub async fn recv(&mut self) -> Option<(EventType, DownloadEvent)> {
        self.rx.recv().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type_method_names() {
        assert_eq!(EventType::DownloadStart.method_name(), "aria2.onDownloadStart");
        assert_eq!(EventType::DownloadComplete.method_name(), "aria2.onDownloadComplete");
        assert_eq!(EventType::DownloadError.method_name(), "aria2.onDownloadError");
    }

    #[test]
    fn test_event_type_from_method() {
        assert!(EventType::from_method("aria2.onDownloadStart").is_some());
        assert!(EventType::from_method("aria2.unknown").is_none());
    }

    #[test]
    fn test_download_event_creation() {
        let event = DownloadEvent::download_start("abc123", vec![serde_json::json!({"path": "/file.iso"})]);
        assert_eq!(event.event_type().unwrap(), EventType::DownloadStart);
        assert_eq!(event.method(), "aria2.onDownloadStart");
        assert_eq!(event.params().len(), 1);
    }

    #[test]
    fn test_download_event_serialization() {
        let event = DownloadEvent::download_error("def456", -1, vec![serde_json::json!({})]);
        let json = event.to_json().unwrap();
        assert!(json.contains("\"method\":\"aria2.onDownloadError\""));
        assert!(json.contains("\"errorCode\":-1"));
    }

    #[test]
    fn test_all_event_constructors() {
        let _ = DownloadEvent::download_start("g1", vec![]);
        let _ = DownloadEvent::download_pause("g2");
        let _ = DownloadEvent::download_stop("g3", vec![]);
        let _ = DownloadEvent::download_complete("g4", vec![]);
        let _ = DownloadEvent::download_error("g5", 0, vec![]);
        let _ = DownloadEvent::bt_download_complete("g6", vec![]);
        let _ = DownloadEvent::bt_download_error("g7", 0, vec![]);
    }

    #[tokio::test]
    async fn test_publisher_subscribe_publish() {
        let publisher = EventPublisher::new(16);
        let mut rx = publisher.subscribe("client-1", None).await;

        let event = DownloadEvent::download_start("test-gid", vec![]);
        let count = publisher.publish(EventType::DownloadStart, event).unwrap();
        assert_eq!(count, 1);

        let (et, received) = rx.recv().await.unwrap();
        assert_eq!(et, EventType::DownloadStart);
        assert_eq!(received.method(), "aria2.onDownloadStart");
    }

    #[tokio::test]
    async fn test_publisher_unsubscribe() {
        let publisher = EventPublisher::new(16);
        publisher.subscribe("client-1", None).await;
        assert_eq!(publisher.subscriber_count().await, 1);

        publisher.unsubscribe("client-1").await;
        assert_eq!(publisher.subscriber_count().await, 0);
    }

    #[tokio::test]
    async fn test_publisher_multiple_subscribers() {
        let publisher = EventPublisher::new(16);
        let _rx1 = publisher.subscribe("c1", None).await;
        let _rx2 = publisher.subscribe("c2", None).await;
        let _rx3 = publisher.subscribe("c3", None).await;
        assert_eq!(publisher.subscriber_count().await, 3);
    }

    #[test]
    fn test_ws_session() {
        let (tx, rx) = broadcast::channel::<(EventType, DownloadEvent)>(4);
        let session = WsSession::new("session-abc", rx);
        assert_eq!(session.id(), "session-abc");
        drop(tx);
    }

    #[test]
    fn test_default_publisher() {
        use futures::future::FutureExt;
        let p = EventPublisher::default();
        assert_eq!(p.subscriber_count().now_or_never().map(|c| c).unwrap(), 0);
    }
}
