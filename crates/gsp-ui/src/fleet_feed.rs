//! Bridges `gsp-aggregator`'s `GET /fleet/subscribe` (SSE, slice 11a) to
//! `gsp-ui`'s own browser-facing WebSocket ([`crate::ws`], slice 11d) — a
//! single shared subscription fanned out to every connected browser, rather
//! than one aggregator connection per browser tab. Reuses the SSE
//! hand-parsing shape `gsp`'s own `controller_client` already uses (a
//! chunked-body loop splitting on blank lines) — the same reasoning applies:
//! this is control-plane, human-paced traffic, not worth a dependency.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const UPDATES_CAPACITY: usize = 16;

/// Holds the most recently received fleet view (raw JSON text, passed
/// through unparsed — `gsp-ui` never needs to interpret it, only relay it)
/// plus a broadcast of the same for live subscribers.
pub struct FleetFeed {
    latest: RwLock<Option<String>>,
    pub updates: tokio::sync::broadcast::Sender<String>,
}

impl FleetFeed {
    pub fn new() -> Arc<Self> {
        let (updates, _rx) = tokio::sync::broadcast::channel(UPDATES_CAPACITY);
        Arc::new(FleetFeed {
            latest: RwLock::new(None),
            updates,
        })
    }

    /// The current view, if a subscription has ever received one — handed
    /// to a browser immediately on connect, before it's had a chance to
    /// receive anything from `updates`.
    pub fn latest(&self) -> Option<String> {
        self.latest
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `pub(crate)` rather than private: only `subscribe_once` calls this in
    /// production, but `crate::ws`'s tests drive it directly too, to exercise
    /// the browser-facing side without needing a real aggregator connection.
    pub(crate) fn set_latest(&self, view: String) {
        *self.latest.write().unwrap_or_else(PoisonError::into_inner) = Some(view.clone());
        // No subscribers connected right now is not an error — `latest`
        // still holds it for whoever connects next.
        let _ = self.updates.send(view);
    }
}

/// Runs forever: subscribes to `{base_url}/fleet/subscribe`, relays every
/// event into `feed`, reconnects with capped exponential backoff on
/// disconnect. `tokio::spawn`ed from `main.rs`; never returns under normal
/// operation.
pub async fn run(base_url: String, token: Option<String>, feed: Arc<FleetFeed>) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match subscribe_once(&base_url, token.as_deref(), &feed).await {
            Ok(()) => {
                backoff = RECONNECT_MIN;
                tracing::warn!(
                    aggregator = %base_url,
                    "fleet subscribe stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    aggregator = %base_url, error = ?e,
                    "fleet subscribe connection failed; the last known view stays available while retrying"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn subscribe_once(
    base_url: &str,
    token: Option<&str>,
    feed: &FleetFeed,
) -> anyhow::Result<()> {
    let url = format!("{base_url}/fleet/subscribe");
    let mut req = gsp_http::client().get(&url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connecting to {url}: {}", gsp_http::error_chain(&e)))?;
    if !resp.status().is_success() {
        anyhow::bail!("aggregator {url} returned {}", resp.status());
    }
    tracing::info!(aggregator = %base_url, "subscribed to aggregator fleet updates");

    let mut buf = String::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| {
            anyhow::anyhow!(
                "reading fleet subscribe stream from {base_url}: {}",
                gsp_http::error_chain(&e)
            )
        })?;
        let Some(bytes) = chunk else {
            return Ok(()); // server closed the stream
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(end) = buf.find("\n\n") {
            let event = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(data) = parse_sse_data(&event) {
                feed.set_latest(data);
            }
        }
    }
}

/// Extracts one SSE event block's `data:` line. `None` for anything that
/// isn't a data event (a keep-alive comment) — ignored, not an error.
fn parse_sse_data(event: &str) -> Option<String> {
    event
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))
        .map(|v| v.trim_start().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_data_event() {
        assert_eq!(parse_sse_data("data: [1,2,3]"), Some("[1,2,3]".to_string()));
    }

    #[test]
    fn a_keep_alive_comment_is_not_a_data_event() {
        assert_eq!(parse_sse_data(": keep-alive"), None);
    }

    #[tokio::test]
    async fn set_latest_updates_the_cache_and_notifies_subscribers() {
        let feed = FleetFeed::new();
        assert!(feed.latest().is_none());

        let mut rx = feed.updates.subscribe();
        feed.set_latest("[]".to_string());

        assert_eq!(feed.latest(), Some("[]".to_string()));
        assert_eq!(rx.recv().await.unwrap(), "[]");
    }
}
