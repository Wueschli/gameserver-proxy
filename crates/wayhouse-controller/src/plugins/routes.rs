//! Plugin routes, materialised (`docs/plugins.md` "Routes"; design #236).
//!
//! Plugins declare `{host, backend}` entries; the controller does not rewrite the operator's
//! config. It publishes the entries of every enabled install as an *overlay* and each proxy
//! appends them to the listeners that opted in (`plugin_routes:` in the config), after the
//! operator's own routes, so operator routes always win. Two installs claiming the same
//! hostname: the earlier install wins and the later claim is reported, not applied.

use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use super::api::PluginsState;
use super::{InstallRecord, PluginStore, PluginStoreError};
use wayhouse_plugin_host::RouteEntry;

/// One materialised route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayRoute {
    pub host: String,
    pub backend: String,
    /// `plugin:<install id>`.
    pub owner: String,
    /// The plugin's install name, for display.
    pub plugin: String,
}

/// A claim that lost to an earlier install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub host: String,
    pub backend: String,
    pub owner: String,
    pub plugin: String,
    /// The owner that holds the hostname.
    pub held_by: String,
}

/// What the controller publishes to proxies and shows operators.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlay {
    /// In match order: exact names before suffix patterns, longer suffixes first.
    pub routes: Vec<OverlayRoute>,
    pub conflicts: Vec<Conflict>,
}

/// More specific first: exact names, then `*.suffix` by descending suffix length.
fn specificity(host: &str) -> (u8, std::cmp::Reverse<usize>) {
    if host.starts_with("*.") {
        (1, std::cmp::Reverse(host.len()))
    } else {
        (0, std::cmp::Reverse(0))
    }
}

/// The overlay for `installs` (each with the route set it last committed). Disabled
/// installs publish nothing. The earlier install (by creation time, then id) wins a
/// hostname.
pub fn materialise(mut installs: Vec<(InstallRecord, Vec<RouteEntry>)>) -> Overlay {
    installs.sort_by(|a, b| (a.0.created_at, &a.0.id).cmp(&(b.0.created_at, &b.0.id)));
    let mut held: BTreeMap<String, String> = BTreeMap::new();
    let mut out = Overlay::default();
    for (rec, entries) in installs {
        if !rec.enabled {
            continue;
        }
        let owner = format!("plugin:{}", rec.id);
        for e in entries {
            match held.get(&e.host) {
                Some(winner) if *winner != owner => out.conflicts.push(Conflict {
                    host: e.host,
                    backend: e.backend,
                    owner: owner.clone(),
                    plugin: rec.name.clone(),
                    held_by: winner.clone(),
                }),
                _ => {
                    held.insert(e.host.clone(), owner.clone());
                    out.routes.push(OverlayRoute {
                        host: e.host,
                        backend: e.backend,
                        owner: owner.clone(),
                        plugin: rec.name.clone(),
                    });
                }
            }
        }
    }
    // Stable: ties keep install order, which keeps the output deterministic.
    out.routes.sort_by_key(|r| specificity(&r.host));
    out
}

impl PluginStore {
    /// The overlay as this replica sees it now.
    pub fn overlay(&self) -> Result<Overlay, PluginStoreError> {
        let mut installs = Vec::new();
        for rec in self.list()? {
            let routes = if rec.enabled {
                self.routes(&rec.id)?
            } else {
                Vec::new()
            };
            installs.push((rec, routes));
        }
        Ok(materialise(installs))
    }
}

/// `GET /plugin-routes`: the current overlay.
pub async fn show(State(st): State<PluginsState>) -> Response {
    match st.store.overlay() {
        Ok(o) => Json(o).into_response(),
        Err(e) => super::api::internal(e).into_response(),
    }
}

/// `GET /plugin-routes/subscribe`: the overlay now, then again whenever it changes.
pub async fn subscribe(
    State(st): State<PluginsState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let store = st.store.clone();
    tokio::spawn(async move {
        let mut changes = store.watch();
        let mut last: Option<Overlay> = None;
        loop {
            match store.overlay() {
                Ok(now) if last.as_ref() != Some(&now) => {
                    let Ok(doc) = serde_json::to_string(&now) else {
                        return;
                    };
                    if tx.send(doc).await.is_err() {
                        return;
                    }
                    last = Some(now);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not read the plugin route overlay"),
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx).map(|doc| Ok(Event::default().data(doc))))
        .keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayhouse_plugin_host::Capabilities;

    fn rec(id: &str, name: &str, at: u64, enabled: bool) -> InstallRecord {
        InstallRecord {
            id: id.into(),
            name: name.into(),
            sha256: "0".repeat(64),
            size: 3,
            approved: Capabilities::parse(br#"{"log":true}"#).unwrap(),
            config: serde_json::json!({}),
            enabled,
            created_at: at,
            created_by: None,
            webhook: None,
        }
    }

    fn e(host: &str, backend: &str) -> RouteEntry {
        RouteEntry {
            host: host.into(),
            backend: backend.into(),
        }
    }

    #[test]
    fn the_earlier_install_wins_a_hostname_and_the_conflict_is_reported() {
        let o = materialise(vec![
            (
                rec("0000000000000002", "later", 20, true),
                vec![
                    e("a.example.com", "10.0.0.2:1"),
                    e("b.example.com", "10.0.0.2:1"),
                ],
            ),
            (
                rec("0000000000000001", "earlier", 10, true),
                vec![e("a.example.com", "10.0.0.1:1")],
            ),
        ]);
        let hosts: Vec<_> = o
            .routes
            .iter()
            .map(|r| (r.host.as_str(), r.plugin.as_str()))
            .collect();
        assert_eq!(
            hosts,
            [("a.example.com", "earlier"), ("b.example.com", "later")]
        );
        assert_eq!(o.conflicts.len(), 1);
        assert_eq!(o.conflicts[0].plugin, "later");
        assert_eq!(o.conflicts[0].held_by, "plugin:0000000000000001");
    }

    #[test]
    fn disabled_installs_publish_nothing_and_exact_names_come_before_suffixes() {
        let o = materialise(vec![
            (
                rec("0000000000000001", "p", 1, true),
                vec![
                    e("*.example.com", "10.0.0.1:1"),
                    e("*.eu.example.com", "10.0.0.1:2"),
                    e("play.example.com", "10.0.0.1:3"),
                ],
            ),
            (
                rec("0000000000000002", "off", 2, false),
                vec![e("x.example.com", "10.0.0.9:1")],
            ),
        ]);
        let hosts: Vec<_> = o.routes.iter().map(|r| r.host.as_str()).collect();
        assert_eq!(
            hosts,
            ["play.example.com", "*.eu.example.com", "*.example.com"]
        );
    }
}
