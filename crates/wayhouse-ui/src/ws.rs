//! `GET /ws/fleet` (slice 11d) — the browser's live-updates WebSocket, fed
//! by [`crate::fleet_feed`]'s single shared subscription to the
//! aggregator's SSE feed. Gated by [`crate::auth::require_session`] like
//! everything else this process serves to a browser — a WS upgrade request
//! is an ordinary `GET` until the `101` handshake (or, over HTTP/2, a `CONNECT`
//! with `:protocol websocket`), so the session cookie is
//! checked exactly the same way (applied by `crate::api::router`, not here —
//! same "route definitions only" shape as `crate::aggregator_proxy`).

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use tokio::sync::broadcast::error::RecvError;

use crate::api::AppState;

pub fn router() -> Router<AppState> {
    // `any`, not `get`: over HTTP/2 (`--tls-cert`, ALPN h2) a browser opens the
    // socket as an extended `CONNECT` (RFC 8441), which `axum::serve` advertises.
    Router::new().route("/ws/fleet", any(ws_handler))
}

/// Close code 1008 (policy violation): the session behind this socket is gone.
const CLOSE_POLICY: u16 = 1008;

async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // `None` when auth is off (nothing to re-check). The upgrade already passed
    // the session gate, so with auth on this is the id of a session that was
    // valid a moment ago.
    let session_id = state
        .auth_configured()
        .then(|| crate::api::session_id_from_headers(&headers))
        .flatten();
    ws.on_upgrade(move |socket| handle_socket(socket, state, session_id))
}

/// The session was valid at the upgrade; a socket may not outlive it. The check
/// peeks ([`crate::session::SessionStore::is_live`]) rather than refreshing, so
/// holding the socket open is not "activity": logout, the idle timeout, the
/// absolute max age and eviction all end the feed like they end every other
/// request. Run on a timer and before every outgoing message.
fn session_ok(state: &AppState, session_id: &Option<String>) -> bool {
    match session_id {
        Some(id) => state.sessions.is_live(id),
        None => !state.auth_configured(),
    }
}

async fn close_policy(socket: &mut WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: CLOSE_POLICY,
            reason: "session ended".into(),
        })))
        .await;
}

async fn handle_socket(mut socket: WebSocket, state: AppState, session_id: Option<String>) {
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
        if !session_ok(&state, &session_id) {
            return close_policy(&mut socket).await;
        }
        if socket.send(Message::Text(view.into())).await.is_err() {
            return;
        }
    }

    let mut updates = feed.updates.subscribe();
    let mut recheck = tokio::time::interval(state.ws_session_recheck);
    recheck.tick().await; // the first tick is immediate; the upgrade just checked
    loop {
        tokio::select! {
            _ = recheck.tick() => {
                if !session_ok(&state, &session_id) {
                    return close_policy(&mut socket).await;
                }
            }
            update = updates.recv() => match update {
                Ok(view) => {
                    if !session_ok(&state, &session_id) {
                        return close_policy(&mut socket).await;
                    }
                    if socket.send(Message::Text(view.into())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // This is state, not a log — catching up means "send
                    // whatever's current now", not replaying what was
                    // missed (same reasoning as wayhouse-controller's
                    // subscribe_worker and wayhouse-aggregator's own
                    // subscribe_fleet_worker).
                    if let Some(view) = feed.latest() {
                        if !session_ok(&state, &session_id) {
                            return close_policy(&mut socket).await;
                        }
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

    /// `axum::serve` advertises HTTP/2 extended CONNECT (RFC 8441), so a browser
    /// that loaded the UI over h2 (`--tls-cert`) opens this socket as `CONNECT`
    /// with `:protocol websocket` on the same connection, not as a `GET` upgrade —
    /// through the same session gate, with its cookie possibly split across
    /// several `cookie` headers. h2c with prior knowledge here: the same HTTP/2
    /// path, without TLS.
    #[tokio::test]
    async fn an_http2_websocket_gets_the_current_view() {
        use hyper::ext::Protocol;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        let feed = crate::fleet_feed::FleetFeed::new();
        feed.set_latest(r#"[{"instance":"h2"}]"#.to_string());
        let state = crate::api::AppState::new(Some("secret".into())).with_fleet_feed(feed);
        let session = state.sessions.create(crate::session::Session {
            role: crate::role::Role::Viewer,
            username: None,
        });
        let app = crate::api::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut send, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake::<_, http_body_util::Empty<bytes::Bytes>>(TokioIo::new(tcp))
            .await
            .unwrap();
        tokio::spawn(conn);
        let connect = |cookies: &[String]| {
            let mut req = hyper::Request::builder()
                .method(hyper::Method::CONNECT)
                .uri(format!("http://{addr}/ws/fleet"))
                .header("sec-websocket-version", "13");
            for cookie in cookies {
                req = req.header("cookie", cookie);
            }
            let mut req = req.body(http_body_util::Empty::new()).unwrap();
            req.extensions_mut()
                .insert(Protocol::from_static("websocket"));
            req
        };

        let resp = send.send_request(connect(&[])).await.unwrap();
        assert_eq!(resp.status(), hyper::StatusCode::UNAUTHORIZED);

        let cookies = [
            "other_app=1".to_string(),
            format!("{}={session}", crate::api::SESSION_COOKIE),
        ];
        let resp = send.send_request(connect(&cookies)).await.unwrap();
        assert_eq!(resp.status(), hyper::StatusCode::OK);

        let upgraded = hyper::upgrade::on(resp).await.unwrap();
        let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            TokioIo::new(upgraded),
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg.into_text().unwrap(), r#"[{"instance":"h2"}]"#);
    }

    /// A server with auth on, a live session, and a feed holding one view;
    /// the client connects with that session's cookie. `recheck` is how often
    /// the open socket re-validates the session.
    async fn connect_authed(
        recheck: std::time::Duration,
    ) -> (
        crate::api::AppState,
        std::sync::Arc<crate::fleet_feed::FleetFeed>,
        String,
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let feed = crate::fleet_feed::FleetFeed::new();
        feed.set_latest(r#"[{"instance":"a"}]"#.to_string());
        let state = crate::api::AppState::new(Some("secret".into()))
            .with_fleet_feed(feed.clone())
            .with_ws_session_recheck(recheck);
        let id = state.sessions.create(crate::session::Session {
            role: crate::role::Role::Viewer,
            username: None,
        });
        let app = crate::api::router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut req = format!("ws://{addr}/ws/fleet")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "cookie",
            format!("{}={id}", crate::api::SESSION_COOKIE)
                .parse()
                .unwrap(),
        );
        let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        // The immediate current-view message proves the socket is established.
        let first = ws.next().await.unwrap().unwrap();
        assert_eq!(first.into_text().unwrap(), r#"[{"instance":"a"}]"#);
        (state, feed, id, ws)
    }

    /// The next frame is a policy-violation close (or the stream just ends
    /// within the deadline) — never another fleet view.
    async fn expect_policy_close(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        match tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
            .await
            .expect("socket stayed open after its session ended")
        {
            Some(Ok(m)) => {
                let tokio_tungstenite::tungstenite::Message::Close(Some(frame)) = m else {
                    panic!("expected a close frame, got {m:?}");
                };
                assert_eq!(frame.code, CloseCode::Policy);
            }
            other => panic!("expected a policy close frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_revoked_session_closes_an_open_socket_on_the_next_check() {
        let (state, _feed, id, mut ws) = connect_authed(std::time::Duration::from_millis(50)).await;
        state.sessions.revoke(&id); // what `POST /ui/logout` does
        expect_policy_close(&mut ws).await;
    }

    #[tokio::test]
    async fn an_update_is_not_sent_once_the_session_is_gone() {
        // Timer effectively off: only the per-message check can catch this.
        let (state, feed, id, mut ws) = connect_authed(std::time::Duration::from_secs(3600)).await;
        state.sessions.revoke(&id);
        feed.set_latest(r#"[{"instance":"secret"}]"#.to_string());
        expect_policy_close(&mut ws).await;
    }

    /// Opens a socket on a fresh session under `limits`, past the initial view.
    async fn connect_with_limits(
        limits: crate::session::SessionLimits,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let feed = crate::fleet_feed::FleetFeed::new();
        feed.set_latest(r#"[{"instance":"a"}]"#.to_string());
        let state = crate::api::AppState::new(Some("secret".into()))
            .with_session_limits(limits)
            .with_fleet_feed(feed)
            .with_ws_session_recheck(std::time::Duration::from_millis(50));
        let id = state.sessions.create(crate::session::Session {
            role: crate::role::Role::Viewer,
            username: None,
        });
        let app = crate::api::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut req = format!("ws://{addr}/ws/fleet")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "cookie",
            format!("{}={id}", crate::api::SESSION_COOKIE)
                .parse()
                .unwrap(),
        );
        let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        let _ = ws.next().await.unwrap().unwrap(); // current view
        ws
    }

    #[tokio::test]
    async fn an_expired_session_closes_an_open_socket() {
        let mut ws = connect_with_limits(crate::session::SessionLimits {
            idle_timeout: std::time::Duration::from_secs(3600),
            max_age: std::time::Duration::from_secs(1),
            max_sessions: 10,
        })
        .await;
        expect_policy_close(&mut ws).await; // max_age passes while it's open
    }

    #[tokio::test]
    async fn a_live_session_keeps_its_socket_open_across_checks() {
        let (_state, feed, _id, mut ws) =
            connect_authed(std::time::Duration::from_millis(30)).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        feed.set_latest(r#"[{"instance":"b"}]"#.to_string());
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg.into_text().unwrap(), r#"[{"instance":"b"}]"#);
    }

    #[tokio::test]
    async fn an_open_socket_does_not_keep_an_idle_session_alive() {
        // The recheck peeks; if it refreshed the idle clock instead, the open
        // socket would defeat `--session-idle-timeout-secs` forever and this
        // would time out rather than see the close.
        let mut ws = connect_with_limits(crate::session::SessionLimits {
            idle_timeout: std::time::Duration::from_secs(1),
            max_age: std::time::Duration::from_secs(3600),
            max_sessions: 10,
        })
        .await;
        expect_policy_close(&mut ws).await;
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
