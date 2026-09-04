//! `GET /ws/fleet` (slice 11d) — the browser's live-updates WebSocket, fed
//! by [`crate::fleet_feed`]'s single shared subscription to the
//! aggregator's SSE feed. Gated by [`crate::auth::require_session`] like
//! everything else this process serves to a browser — a WS upgrade request
//! is an ordinary `GET` until the `101` handshake, so the session cookie is
//! checked exactly the same way (applied by `crate::api::router`, not here —
//! same "route definitions only" shape as `crate::aggregator_proxy`).

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tokio::sync::broadcast::error::RecvError;

use crate::api::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/ws/fleet", get(ws_handler))
}

async fn ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let Some(feed) = state.fleet_feed.clone() else {
        // No --aggregator-url configured at all: nothing to stream. Tell the
        // browser once and close, rather than leaving it hanging.
        let _ = socket
            .send(Message::Text("no aggregator configured".into()))
            .await;
        return;
    };

    // Send the current view immediately, if one exists, so a browser that
    // connects between two aggregator pushes isn't left blank until the
    // next one happens to land.
    if let Some(view) = feed.latest() {
        if socket.send(Message::Text(view.into())).await.is_err() {
            return;
        }
    }

    let mut updates = feed.updates.subscribe();
    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(view) => {
                    if socket.send(Message::Text(view.into())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // This is state, not a log — catching up means "send
                    // whatever's current now", not replaying what was
                    // missed (same reasoning as gsp-controller's
                    // subscribe_worker and gsp-aggregator's own
                    // subscribe_fleet_worker).
                    if let Some(view) = feed.latest() {
                        if socket.send(Message::Text(view.into())).await.is_err() {
                            return;
                        }
                    }
                }
                Err(RecvError::Closed) => return,
            },
            incoming = socket.recv() => match incoming {
                None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                _ => {} // the browser has nothing meaningful to send us; ignore it
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    /// Spins up the real router (not a mock) on an ephemeral port and
    /// connects a real WebSocket client to it — proving the whole bridge,
    /// not just `fleet_feed`'s logic in isolation.
    async fn connect(
        feed: std::sync::Arc<crate::fleet_feed::FleetFeed>,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let state = crate::api::AppState::new(None).with_fleet_feed(feed); // no password: require_session lets everything through
        let app = crate::api::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/fleet"))
            .await
            .unwrap();
        ws
    }

    #[tokio::test]
    async fn a_connecting_browser_gets_the_current_view_immediately() {
        let feed = crate::fleet_feed::FleetFeed::new();
        // Drives the cache the same way `subscribe_once` does on a real
        // aggregator event, without needing a real aggregator connection.
        feed.set_latest(r#"[{"instance":"a"}]"#.to_string());

        let mut ws = connect(feed).await;
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg.into_text().unwrap(), r#"[{"instance":"a"}]"#);
    }

    #[tokio::test]
    async fn a_live_update_after_connecting_is_relayed() {
        let feed = crate::fleet_feed::FleetFeed::new();
        let mut ws = connect(feed.clone()).await;

        feed.set_latest(r#"[{"instance":"b"}]"#.to_string());

        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg.into_text().unwrap(), r#"[{"instance":"b"}]"#);
        let _ = ws.close(None).await;
    }

    #[tokio::test]
    async fn no_aggregator_configured_sends_one_message_then_closes() {
        let state = crate::api::AppState::new(None); // no fleet_feed at all
        let app = crate::api::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/fleet"))
            .await
            .unwrap();

        let msg = ws.next().await.unwrap().unwrap();
        assert_eq!(msg.into_text().unwrap(), "no aggregator configured");
        // The server closes the socket right after; the client's next poll
        // sees either a close frame or the stream simply ending — either is
        // the expected shutdown, not a hang.
        match ws.next().await {
            None => {}
            Some(Ok(m)) => assert!(m.is_close()),
            Some(Err(_)) => {}
        }
    }
}
