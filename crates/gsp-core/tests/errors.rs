//! The data-plane library boundaries return typed errors callers can match on
//! (`ProxyError`, `ListenerError`, `SourceError`), not `anyhow`.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};

use gsp_config::parse_str;
use gsp_core::drain::{Proto, SessionMeta};
use gsp_core::pool::PickError;
use gsp_core::proxy::handle_tcp;
use gsp_core::{ConnTracker, ProxyError, Snapshot};

/// A connected client socket plus its tracker guard, as `handle_tcp` wants.
async fn client_pair() -> (TcpStream, SocketAddr, SocketAddr, gsp_core::ConnGuard) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = l.local_addr().unwrap();
    let c = TcpStream::connect(local).await.unwrap();
    let (_srv, peer) = l.accept().await.unwrap();
    let guard = ConnTracker::new().track(SessionMeta {
        proto: Proto::Tcp,
        listener: "l".into(),
        peer,
        local,
    });
    (c, peer, local, guard)
}

fn snapshot_for(target: SocketAddr) -> Arc<Snapshot> {
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{target}\"]\n\
         listeners:\n  - name: l\n    bind: \"127.0.0.1:0\"\n    pool: p\n"
    );
    Snapshot::from_config(&parse_str(&yaml).unwrap())
}

#[tokio::test]
async fn handle_tcp_reports_a_refused_backend_as_connect() {
    // A port nothing listens on: bind then drop.
    let dead = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    let snap = snapshot_for(dead);
    let pool = snap.pool("p").unwrap();
    let (client, peer, local, guard) = client_pair().await;

    let err = handle_tcp(client, peer, local, None, &guard, &pool)
        .await
        .unwrap_err();
    match &err {
        ProxyError::Connect { addr, .. } => assert_eq!(*addr, dead),
        other => panic!("expected Connect, got {other:?}"),
    }
    assert!(
        err.to_string()
            .starts_with(&format!("connect to backend {dead} failed: ")),
        "{err}"
    );
}

#[tokio::test]
async fn handle_tcp_reports_an_unavailable_pool_as_pick() {
    // Passive feedback: repeated refused connects mark the only backend down,
    // after which the pool has nothing to pick.
    let dead = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    let snap = snapshot_for(dead);
    let pool = snap.pool("p").unwrap();

    for _ in 0..20 {
        let (client, peer, local, guard) = client_pair().await;
        let err = handle_tcp(client, peer, local, None, &guard, &pool)
            .await
            .unwrap_err();
        if let ProxyError::Pick(PickError::NoHealthyBackend(n)) = err {
            assert_eq!(n, "p");
            return;
        }
    }
    panic!("pool never reported NoHealthyBackend");
}
