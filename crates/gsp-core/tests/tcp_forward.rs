//! End-to-end smoke test: bytes flow client -> proxy -> backend -> client.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use gsp_config::parse_str;
use gsp_core::{Runtime, Snapshot};

#[tokio::test]
async fn forwards_tcp_bytes_end_to_end() {
    // Backend: a trivial echo server on an ephemeral port.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = backend.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    // Grab a free port for the proxy listener, then release it.
    let proxy_addr = {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap()
    };

    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let snapshot: Arc<Snapshot> = Snapshot::from_config(&cfg);
    let runtime = Runtime::start(snapshot, 1);

    // Let the listener bind.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    client.write_all(b"hello proxy").await.unwrap();
    let mut buf = [0u8; 11];
    client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello proxy");

    drop(client);
    runtime.shutdown().await;
}
