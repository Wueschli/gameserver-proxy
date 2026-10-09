//! Delivers events to plugins that declared `on_event` (design: the automation hooks addendum).
//!
//! Two kinds exist: `config_revision` (`{"revision": N}`, a new config revision was accepted)
//! and `plugin_changed` (`{"id": "...", "change": "installed|enabled|disabled|deleted"}`). They
//! are hints, not a log: a plugin that must not miss a change reconciles on its timer. An event
//! reaches a plugin through the same bounded hook queue as a webhook, so a slow plugin loses
//! events (counted as `hooks_dropped`) instead of building a backlog, and only the Raft leader
//! delivers.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::broadcast;

use super::runner::{Hook, Runner};
use super::PluginStore;

pub const CONFIG_REVISION: &str = "config_revision";
pub const PLUGIN_CHANGED: &str = "plugin_changed";

/// The kinds a plugin may subscribe to.
pub const KINDS: [&str; 2] = [CONFIG_REVISION, PLUGIN_CHANGED];

/// Sends `kind` to every enabled install that approved it. Each call runs in its own task, so
/// one slow plugin never delays another or the feed.
pub fn deliver(runner: &Arc<Runner>, store: &PluginStore, kind: &str, payload: &serde_json::Value) {
    let Ok(installs) = store.list() else {
        return;
    };
    let payload = payload.to_string().into_bytes();
    for rec in installs {
        if !rec.enabled || !rec.approved.triggers.on_event.iter().any(|k| k == kind) {
            continue;
        }
        let (runner, kind, payload) = (runner.clone(), kind.to_string(), payload.clone());
        drop(tokio::spawn(async move {
            // Not the leader, queue full and plugin errors are all counted or logged by the
            // runner; an event has no caller to tell.
            let _ = runner
                .run_hook(&rec.id, Hook::Event { kind, payload })
                .await;
        }));
    }
}

/// What changed between two listings, as `(id, change)` pairs.
pub fn diff(
    old: &BTreeMap<String, bool>,
    new: &BTreeMap<String, bool>,
) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    for (id, enabled) in new {
        match old.get(id) {
            None => out.push((id.clone(), "installed")),
            Some(was) if was != enabled => {
                out.push((id.clone(), if *enabled { "enabled" } else { "disabled" }));
            }
            Some(_) => {}
        }
    }
    for id in old.keys().filter(|id| !new.contains_key(*id)) {
        out.push((id.clone(), "deleted"));
    }
    out
}

fn snapshot(store: &PluginStore) -> Option<BTreeMap<String, bool>> {
    Some(
        store
            .list()
            .ok()?
            .into_iter()
            .map(|r| (r.id, r.enabled))
            .collect(),
    )
}

/// Feeds the two event kinds until the process ends. `config` carries accepted revisions.
pub fn spawn(
    runner: Arc<Runner>,
    store: PluginStore,
    mut config: broadcast::Receiver<u64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut changes = store.watch();
        let mut known = snapshot(&store).unwrap_or_default();
        loop {
            tokio::select! {
                got = config.recv() => match got {
                    Ok(revision) => deliver(
                        &runner, &store, CONFIG_REVISION,
                        &serde_json::json!({ "revision": revision }),
                    ),
                    // Missed some: one event for the newest is all a hint needs.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let Some(now) = snapshot(&store) else { continue };
                    for (id, change) in diff(&known, &now) {
                        deliver(
                            &runner, &store, PLUGIN_CHANGED,
                            &serde_json::json!({ "id": id, "change": change }),
                        );
                    }
                    known = now;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(items: &[(&str, bool)]) -> BTreeMap<String, bool> {
        items.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn the_diff_names_installs_toggles_and_deletes() {
        let old = m(&[("a", true), ("b", true), ("c", false)]);
        let new = m(&[("a", true), ("b", false), ("c", true), ("d", true)]);
        let mut got = diff(&old, &new);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("b".to_string(), "disabled"),
                ("c".to_string(), "enabled"),
                ("d".to_string(), "installed"),
            ]
        );
        assert_eq!(diff(&new, &m(&[])).len(), 4);
        assert_eq!(
            diff(&m(&[("x", true)]), &m(&[])),
            vec![("x".to_string(), "deleted")]
        );
        assert!(diff(&new, &new).is_empty());
    }
}
