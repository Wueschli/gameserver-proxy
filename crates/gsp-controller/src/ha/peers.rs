//! `--ha-peers` addressing: how a replica names its peers and how every
//! replica-to-replica URL (Raft RPCs, forwarded writes) is built from that.
//!
//! An entry is `id=host:port` (plain HTTP, the original form) or
//! `id=https://host[:port]` / `id=http://…` — a base URL, e.g. a TLS
//! terminator in front of that replica's listener. The address is stored as
//! given (URLs normalised) in the Raft membership, so it only takes effect
//! when the cluster is bootstrapped; see docs/12 "HA over TLS".

use std::collections::BTreeMap;

use openraft::BasicNode;
use reqwest::Url;

use super::NodeId;

/// The URL for `path` on the peer at `addr` (a `BasicNode::addr`).
pub fn peer_url(addr: &str, path: &str) -> String {
    if addr.contains("://") {
        format!("{}{path}", addr.trim_end_matches('/'))
    } else {
        format!("http://{addr}{path}")
    }
}

/// Parse `--ha-peers` into the bootstrap membership.
pub fn parse_peers(raw: &[String]) -> anyhow::Result<BTreeMap<NodeId, BasicNode>> {
    let mut peers = BTreeMap::new();
    for pair in raw {
        let (id, addr) = pair
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--ha-peers entry {pair:?} is not id=host:port"))?;
        let id: NodeId = id
            .parse()
            .map_err(|e| anyhow::anyhow!("--ha-peers entry {pair:?} has an invalid id: {e}"))?;
        let addr = if addr.contains("://") {
            normalise_url(addr)
                .map_err(|why| anyhow::anyhow!("--ha-peers entry {pair:?}: {why}"))?
        } else {
            addr.to_string()
        };
        peers.insert(id, BasicNode::new(addr));
    }
    Ok(peers)
}

/// `scheme://host[:port]` with no trailing slash, or why `addr` can't be one.
fn normalise_url(addr: &str) -> Result<String, String> {
    let url = Url::parse(addr).map_err(|e| format!("not a valid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "scheme must be http or https, not {:?}",
            url.scheme()
        ));
    }
    if url.host_str().is_none() {
        return Err("a host is required".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("user info is not allowed".into());
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err("only scheme://host[:port] is allowed (no path, query or fragment)".into());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    fn err(entry: &str) -> String {
        parse_peers(&raw(&[entry])).unwrap_err().to_string()
    }

    #[test]
    fn legacy_entries_stay_plain_http() {
        assert_eq!(
            peer_url("127.0.0.1:9901", "/raft/vote"),
            "http://127.0.0.1:9901/raft/vote"
        );
        assert_eq!(
            peer_url("[::1]:9911", "/raft/vote"),
            "http://[::1]:9911/raft/vote"
        );
        let peers = parse_peers(&raw(&["1=127.0.0.1:9901"])).unwrap();
        assert_eq!(peers[&1].addr, "127.0.0.1:9901");
    }

    #[test]
    fn url_entries_are_normalised() {
        let peers = parse_peers(&raw(&[
            "1=HTTPS://Localhost:8443/",
            "2=https://ctl.example",
        ]))
        .unwrap();
        assert_eq!(peers[&1].addr, "https://localhost:8443");
        assert_eq!(peers[&2].addr, "https://ctl.example");
        assert_eq!(
            peer_url("https://localhost:8443", "/raft/append"),
            "https://localhost:8443/raft/append"
        );
    }

    #[test]
    fn bad_url_entries_are_rejected() {
        for entry in [
            "1=ftp://h:1",
            "1=https://h:1/x",
            "1=https://h:1/?q=1",
            "1=https://u@h:1",
        ] {
            let e = err(entry);
            assert!(
                e.starts_with(&format!("--ha-peers entry {entry:?}: ")),
                "{entry}: {e}"
            );
        }
    }

    #[test]
    fn entries_without_an_id_are_rejected() {
        assert!(err("127.0.0.1:1").contains("is not id=host:port"));
    }
}
