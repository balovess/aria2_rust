//! Runtime HTTP context supplied by a browser bridge.
//!
//! The browser process is intentionally outside aria2-core. A CDP client or
//! extension can deserialize a [`BrowserContextUpdate`] and publish it here;
//! download requests then read one atomic snapshot immediately before send.

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Credentials and headers shared with an active browser session.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct BrowserContextUpdate {
    /// Complete Cookie header, as emitted by the browser for the target site.
    #[serde(default)]
    pub cookie: Option<String>,
    /// Browser User-Agent. Kept separate because reqwest's client default is
    /// fixed when the client is constructed.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Dynamic headers such as signed access tokens.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

/// Thread-safe last-value-wins browser session context.
#[derive(Clone, Debug, Default)]
pub struct BrowserContext {
    current: Arc<RwLock<BrowserContextUpdate>>,
}

impl BrowserContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the complete browser snapshot. Partial updates should be
    /// merged by the bridge before publishing so stale tokens are removed.
    pub fn replace(&self, update: BrowserContextUpdate) {
        if let Ok(mut current) = self.current.write() {
            *current = update;
        }
    }

    /// Publish a JSON snapshot received from a CDP client or extension.
    pub fn replace_json(&self, json: &str) -> Result<(), serde_json::Error> {
        let update = serde_json::from_str(json)?;
        self.replace(update);
        Ok(())
    }

    pub fn snapshot(&self) -> BrowserContextUpdate {
        self.current
            .read()
            .map(|value| value.clone())
            .unwrap_or_default()
    }

    /// Return headers to apply to one request. Explicit request headers win;
    /// this prevents a browser snapshot from silently overriding aria2
    /// configuration supplied for a specific download.
    pub fn headers_for(&self, explicit: &[(String, String)]) -> Vec<(String, String)> {
        let snapshot = self.snapshot();
        let mut result = snapshot.headers;
        if let Some(cookie) = snapshot.cookie.filter(|value| !value.is_empty()) {
            result.push(("Cookie".to_string(), cookie));
        }
        if let Some(user_agent) = snapshot.user_agent.filter(|value| !value.is_empty()) {
            result.push(("User-Agent".to_string(), user_agent));
        }
        result.retain(|(name, _)| {
            !explicit
                .iter()
                .any(|(explicit_name, _)| explicit_name.eq_ignore_ascii_case(name))
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_is_atomic_and_explicit_headers_win() {
        let context = BrowserContext::new();
        context.replace(BrowserContextUpdate {
            cookie: Some("sid=browser".into()),
            user_agent: Some("Browser UA".into()),
            headers: vec![("X-Signature".into(), "old".into())],
        });
        context.replace(BrowserContextUpdate {
            cookie: Some("sid=fresh".into()),
            user_agent: Some("Fresh UA".into()),
            headers: vec![("X-Signature".into(), "fresh".into())],
        });

        let headers = context.headers_for(&[("Cookie".into(), "configured".into())]);
        assert!(
            !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Cookie"))
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "X-Signature" && value == "fresh")
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "User-Agent" && value == "Fresh UA")
        );
    }

    #[test]
    fn accepts_json_snapshot_from_a_bridge() {
        let context = BrowserContext::new();
        context
            .replace_json(r#"{"cookie":"sid=1","user_agent":"UA","headers":[["X-Token","t"]]}"#)
            .unwrap();
        assert_eq!(context.snapshot().cookie.as_deref(), Some("sid=1"));
        assert_eq!(context.snapshot().headers[0].1, "t");
    }
}
