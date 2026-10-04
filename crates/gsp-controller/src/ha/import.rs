//! Importing pre-HA data when a controller upgrades to `--ha-peers` ("Import",
//! `docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//!
//! A node starting with `--ha-peers` whose `peers/`, `proxy-peers/` or
//! `tunnel-addresses/` hold data but no `applied_index` marker has pre-HA
//! data: [`set_aside_pre_ha`] renames each such directory to `<dir>.pre-ha`
//! (never deleting it) so the state machine starts on fresh databases. The
//! cluster's leader later picks one node's set-aside data as the source
//! (`ha::init`), serves it as [`ImportContent`] over `GET /raft/pre-ha`, and
//! proposes it as one `Import` entry that [`apply_import`] applies once.

// `apply_import` returns `openraft`'s `StorageIOError`, sized by its own
// variants — not something this crate can shrink (as in `apply_registry`).
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use openraft::StorageIOError;
use serde::{Deserialize, Serialize};

use super::cluster_state::ClusterState;
use super::{NodeId, WriteResponse};
use crate::addresses::{AddressBook, BookSnapshot, Entry, Network, Rejection};
use crate::peers::PeerRegistration;
use crate::proxy_peers::ProxyRegistration;
use crate::registry::{Registration, RegistrySnapshot, RegistryState};
use crate::store::Store;

const SUFFIX: &str = ".pre-ha";

/// What a node's set-aside pre-HA data holds (all zero = none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreHaSummary {
    pub origins: usize,
    pub proxies: usize,
    pub addresses: usize,
}

impl PreHaSummary {
    pub fn is_empty(&self) -> bool {
        *self == PreHaSummary::default()
    }
}

/// Everything the importing entry carries: the current registration per name
/// of each registry, the address-book entries, the source's tunnel network
/// and each registry's last revision number (the imported logs continue after
/// it, so a subscriber's old `since` cursor stays valid).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportContent {
    /// The node the data came from.
    pub source: NodeId,
    pub origins: Vec<PeerRegistration>,
    pub proxies: Vec<ProxyRegistration>,
    pub book: Vec<Entry>,
    pub network: Option<String>,
    pub origins_last_revision: u64,
    pub proxies_last_revision: u64,
}

/// This node's own pre-HA data, as the `/raft/*` routes and the leader's
/// initialization need it.
#[derive(Debug, Clone, Default)]
pub struct LocalPreHa {
    pub summary: PreHaSummary,
    pub data_dir: PathBuf,
    /// This node's `--tunnel-network`, the network its data was allocated in.
    pub network: Option<Network>,
}

fn aside(dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_owned();
    name.push(SUFFIX);
    PathBuf::from(name)
}

fn is_pre_ha_store(dir: &Path) -> anyhow::Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let store = Store::open(dir).with_context(|| format!("opening {dir:?}"))?;
    Ok(store.current_revision()?.is_some() && store.applied_index()?.is_none())
}

fn is_pre_ha_book(dir: &Path) -> anyhow::Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let book = AddressBook::open(dir, None).with_context(|| format!("opening {dir:?}"))?;
    Ok(!book.entries().map_err(|e| anyhow!("{e}"))?.is_empty()
        && book
            .applied_index()
            .map_err(|e| anyhow!("{}", e.0))?
            .is_none())
}

/// Renames each pre-HA registry directory under `data_dir` to `<dir>.pre-ha`
/// and returns what the set-aside directories hold — including ones set aside
/// by an earlier start. Call before any of the databases is opened.
pub fn set_aside_pre_ha(data_dir: &Path) -> anyhow::Result<PreHaSummary> {
    for (name, is_pre_ha) in [
        (
            "peers",
            is_pre_ha_store as fn(&Path) -> anyhow::Result<bool>,
        ),
        ("proxy-peers", is_pre_ha_store),
        ("tunnel-addresses", is_pre_ha_book),
    ] {
        let dir = data_dir.join(name);
        if !is_pre_ha(&dir)? {
            continue;
        }
        let target = aside(&dir);
        if target.exists() {
            anyhow::bail!(
                "{dir:?} holds pre-HA data but {target:?} already exists; move one of them away \
                 (or delete {target:?} if it was already imported) and start again"
            );
        }
        std::fs::rename(&dir, &target)
            .with_context(|| format!("setting {dir:?} aside as {target:?}"))?;
        tracing::warn!(
            from = %dir.display(),
            to = %target.display(),
            "set pre-HA registry data aside; it is imported when the cluster initializes"
        );
    }
    Ok(PreHaSummary {
        origins: count_registry(&aside(&data_dir.join("peers")))?,
        proxies: count_registry(&aside(&data_dir.join("proxy-peers")))?,
        addresses: count_book(&aside(&data_dir.join("tunnel-addresses")))?,
    })
}

fn count_registry(dir: &Path) -> anyhow::Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let store = Store::open(dir).with_context(|| format!("opening {dir:?}"))?;
    Ok(store.db().open_tree("current")?.len())
}

fn count_book(dir: &Path) -> anyhow::Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let book = AddressBook::open(dir, None).with_context(|| format!("opening {dir:?}"))?;
    Ok(book.entries().map_err(|e| anyhow!("{e}"))?.len())
}

/// The current registration per name and the last revision number.
fn read_registry<R: Registration>(dir: &Path) -> anyhow::Result<(Vec<R>, u64)> {
    if !dir.is_dir() {
        return Ok((Vec::new(), 0));
    }
    let store = Store::open(dir).with_context(|| format!("opening {dir:?}"))?;
    let current = store.db().open_tree("current")?;
    let mut regs = Vec::new();
    for item in current.iter() {
        let (name, rev) = item?;
        let rev = u64::from_be_bytes(rev.as_ref().try_into().context("a current pointer")?);
        let bytes = store
            .get(rev)?
            .ok_or_else(|| anyhow!("{dir:?}: current {name:?} points at missing revision {rev}"))?;
        regs.push(serde_json::from_slice(&bytes).with_context(|| format!("{dir:?}: {name:?}"))?);
    }
    Ok((regs, store.current_revision()?.unwrap_or(0)))
}

/// This node's set-aside data as [`ImportContent`], `None` when it has none.
pub fn read_pre_ha(
    data_dir: &Path,
    network: Option<Network>,
    source: NodeId,
) -> anyhow::Result<Option<ImportContent>> {
    let (origins, origins_last_revision) = read_registry(&aside(&data_dir.join("peers")))?;
    let (proxies, proxies_last_revision) = read_registry(&aside(&data_dir.join("proxy-peers")))?;
    let book_dir = aside(&data_dir.join("tunnel-addresses"));
    let book = if book_dir.is_dir() {
        AddressBook::open(&book_dir, None)?
            .entries()
            .map_err(|e| anyhow!("{e}"))?
    } else {
        Vec::new()
    };
    if origins.is_empty() && proxies.is_empty() && book.is_empty() {
        return Ok(None);
    }
    Ok(Some(ImportContent {
        source,
        origins,
        proxies,
        book,
        network: network.map(|n| n.to_string()),
        origins_last_revision,
        proxies_last_revision,
    }))
}

fn io<E: std::error::Error + 'static>(e: E) -> StorageIOError<NodeId> {
    StorageIOError::write_state_machine(&e)
}

/// Installs `regs` as the registry's whole log, numbered from
/// `last_revision + 1`, unless the registry already absorbed `index`.
fn import_registry<R: Registration>(
    registry: &RegistryState<R>,
    regs: &[R],
    last_revision: u64,
    index: u64,
) -> Result<(), StorageIOError<NodeId>> {
    if registry
        .store
        .applied_index()
        .map_err(io)?
        .is_some_and(|applied| applied >= index)
    {
        return Ok(());
    }
    let mut regs: Vec<&R> = regs.iter().collect();
    regs.sort_by(|a, b| a.name().cmp(b.name()));
    let mut snapshot = RegistrySnapshot {
        revisions: Vec::new(),
        current: Vec::new(),
        applied_index: Some(index),
    };
    for (i, reg) in regs.into_iter().enumerate() {
        let revision = last_revision + 1 + i as u64;
        let bytes = serde_json::to_vec(reg).expect("a registration always serializes");
        snapshot.revisions.push((revision, bytes));
        snapshot.current.push((reg.name().to_string(), revision));
    }
    registry.replace(&snapshot).map_err(io)
}

/// Applies `Import` at `index`: once. When the cluster is already initialized
/// (by an earlier `Import` or `SetTunnelNetwork`) the entry is a no-op, so a
/// second leader racing an import cannot import twice. Each database skips
/// its own step when it already absorbed `index`, and the cluster state is
/// recorded last, so a replay after a crash finishes the import.
pub(super) fn apply_import(
    peers: &RegistryState<PeerRegistration>,
    proxy_peers: &RegistryState<ProxyRegistration>,
    book: &AddressBook,
    cluster: &ClusterState,
    content: &ImportContent,
    index: u64,
) -> Result<WriteResponse, StorageIOError<NodeId>> {
    let parsed = content.network.as_deref().map(Network::parse).transpose();
    let skip = match (&parsed, cluster.network().map_err(io)?) {
        (_, Some(_)) => Some(WriteResponse::Recorded),
        (Err(e), None) => Some(WriteResponse::Rejected(Rejection::InvalidNetwork(
            e.clone(),
        ))),
        (Ok(_), None) => None,
    };
    if let Some(response) = skip {
        peers.store.mark_applied(index).map_err(io)?;
        proxy_peers.store.mark_applied(index).map_err(io)?;
        book.mark_applied(index).map_err(io)?;
        return Ok(response);
    }
    if book.applied_index().map_err(io)?.is_none_or(|a| a < index) {
        book.replace(&BookSnapshot {
            entries: content.book.clone(),
            applied_index: Some(index),
            last_outcome: None,
        })
        .map_err(io)?;
    }
    import_registry(
        peers,
        &content.origins,
        content.origins_last_revision,
        index,
    )?;
    import_registry(
        proxy_peers,
        &content.proxies,
        content.proxies_last_revision,
        index,
    )?;
    cluster
        .record(parsed.expect("checked above"), index)
        .map_err(io)?;
    Ok(WriteResponse::Recorded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addresses::{Assignment, Role};
    use crate::store::{reopen_when_unlocked, retry_when_unlocked};
    use std::sync::Arc;

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn origin(name: &str, address: &str) -> PeerRegistration {
        PeerRegistration {
            name: name.into(),
            pubkey: KEY.into(),
            endpoint: None,
            backends: vec![format!("{address}:25565")],
            tunnel_address: Some(address.into()),
        }
    }

    /// A pre-HA data dir the way the old single node left it: three origins
    /// (one re-registered, so the log head is 4) and their addresses.
    fn pre_ha_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("tunnel-addresses"),
                Some(Network::parse("10.60.0.0/24").unwrap()),
            )
            .unwrap(),
        );
        let peers = crate::peers::api::PeersState::new(
            Arc::new(Store::open(&dir.path().join("peers")).unwrap()),
            None,
            book.clone(),
        );
        for (i, name) in ["a", "b", "c", "a"].into_iter().enumerate() {
            let address = format!("10.60.0.{}", 1 + i.min(2));
            let reg = origin(name, &address);
            if i < 3 {
                book.claim(Role::Origin, name, None, 1_000).unwrap();
            }
            peers.register_applied(&reg, None).unwrap();
        }
        dir
    }

    #[test]
    fn set_aside_moves_only_unmarked_non_empty_dirs() {
        let dir = pre_ha_dir();
        // An empty registry and a marked (replicated) one stay where they are.
        drop(Store::open(&dir.path().join("proxy-peers")).unwrap());
        let marked = dir.path().join("peers-marked");
        Store::open(&marked)
            .unwrap()
            .put_applied(b"{}".to_vec(), 7)
            .unwrap();
        assert!(!reopen_when_unlocked(|| is_pre_ha_store(&marked)));
        assert!(!reopen_when_unlocked(|| is_pre_ha_store(
            &dir.path().join("proxy-peers")
        )));

        let summary = reopen_when_unlocked(|| set_aside_pre_ha(dir.path()));
        assert_eq!(
            summary,
            PreHaSummary {
                origins: 3,
                proxies: 0,
                addresses: 3
            }
        );
        assert!(!dir.path().join("peers").exists());
        assert!(dir.path().join("peers.pre-ha").is_dir());
        assert!(dir.path().join("tunnel-addresses.pre-ha").is_dir());
        assert!(dir.path().join("proxy-peers").is_dir());
        assert!(!dir.path().join("proxy-peers.pre-ha").exists());

        // A second start (fresh dirs in place) still reports the set-aside data.
        drop(Store::open(&dir.path().join("peers")).unwrap());
        assert_eq!(
            reopen_when_unlocked(|| set_aside_pre_ha(dir.path())),
            summary
        );
    }

    #[test]
    fn set_aside_refuses_to_overwrite_an_earlier_set_aside() {
        let dir = pre_ha_dir();
        reopen_when_unlocked(|| set_aside_pre_ha(dir.path()));
        // The old controller ran again and wrote new data.
        let again = pre_ha_dir();
        std::fs::rename(again.path().join("peers"), dir.path().join("peers")).unwrap();
        let e = retry_when_unlocked(|| set_aside_pre_ha(dir.path()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("already exists"), "{e}");
    }

    #[test]
    fn read_pre_ha_reads_the_current_registrations_and_the_head() {
        let dir = pre_ha_dir();
        let net = Some(Network::parse("10.60.0.0/24").unwrap());
        assert!(
            reopen_when_unlocked(|| read_pre_ha(dir.path(), net, 1)).is_none(),
            "nothing set aside yet"
        );
        reopen_when_unlocked(|| set_aside_pre_ha(dir.path()));
        let content = reopen_when_unlocked(|| read_pre_ha(dir.path(), net, 1)).unwrap();
        let names: Vec<_> = content.origins.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(content.origins_last_revision, 4);
        assert_eq!(content.proxies_last_revision, 0);
        assert_eq!(content.book.len(), 3);
        assert_eq!(content.network.as_deref(), Some("10.60.0.0/24"));
        assert_eq!(content.source, 1);
    }

    /// The state machine's own databases, empty and unreplicated.
    struct Target {
        peers: RegistryState<PeerRegistration>,
        proxies: RegistryState<ProxyRegistration>,
        book: Arc<AddressBook>,
        cluster: ClusterState,
        _dir: tempfile::TempDir,
    }

    fn target() -> Target {
        let dir = tempfile::tempdir().unwrap();
        let book = Arc::new(AddressBook::open(&dir.path().join("book"), None).unwrap());
        let store = |sub: &str| Arc::new(Store::open(&dir.path().join(sub)).unwrap());
        Target {
            peers: RegistryState::new(store("peers"), None, book.clone()),
            proxies: RegistryState::new(store("proxies"), None, book.clone()),
            cluster: ClusterState::open(&sled::open(dir.path().join("ha")).unwrap()).unwrap(),
            book,
            _dir: dir,
        }
    }

    fn content() -> ImportContent {
        let assignment = |a: &str| Assignment {
            address: a.parse().unwrap(),
            first_seen: 10,
            last_seen: 20,
        };
        ImportContent {
            source: 1,
            origins: vec![origin("b", "10.60.0.2"), origin("a", "10.60.0.1")],
            proxies: Vec::new(),
            book: vec![
                Entry {
                    role: Role::Origin,
                    name: "a".into(),
                    assignment: assignment("10.60.0.1"),
                },
                Entry {
                    role: Role::Origin,
                    name: "b".into(),
                    assignment: assignment("10.60.0.2"),
                },
            ],
            network: Some("10.60.0.0/24".into()),
            origins_last_revision: 40,
            proxies_last_revision: 0,
        }
    }

    fn apply(t: &Target, content: &ImportContent, index: u64) -> WriteResponse {
        apply_import(&t.peers, &t.proxies, &t.book, &t.cluster, content, index).unwrap()
    }

    #[test]
    fn import_applies_once() {
        let t = target();
        assert_eq!(apply(&t, &content(), 5), WriteResponse::Recorded);
        let before = (t.peers.snapshot().unwrap(), t.book.snapshot().unwrap());

        let mut other = content();
        other.origins = vec![origin("z", "10.60.0.9")];
        other.book.clear();
        assert_eq!(apply(&t, &other, 6), WriteResponse::Recorded);
        assert_eq!(
            t.peers.current_for("z").unwrap(),
            None,
            "the second import is a no-op"
        );
        assert_eq!(t.peers.snapshot().unwrap().current, before.0.current);
        assert_eq!(t.book.entries().unwrap().len(), 2);
        assert_eq!(
            t.book.applied_index().unwrap(),
            Some(6),
            "but it advances the index"
        );
        assert_eq!(
            t.cluster.network().unwrap(),
            Some(Some(Network::parse("10.60.0.0/24").unwrap()))
        );
    }

    #[test]
    fn a_replayed_import_changes_nothing() {
        let t = target();
        apply(&t, &content(), 5);
        let before = (t.peers.snapshot().unwrap(), t.book.snapshot().unwrap());
        assert_eq!(apply(&t, &content(), 5), WriteResponse::Recorded);
        assert_eq!(
            (t.peers.snapshot().unwrap(), t.book.snapshot().unwrap()),
            before
        );
    }

    #[test]
    fn a_crash_between_the_book_and_the_cluster_state_finishes_on_replay() {
        let t = target();
        // The book step ran, then the process died.
        t.book
            .replace(&BookSnapshot {
                entries: content().book,
                applied_index: Some(5),
                last_outcome: None,
            })
            .unwrap();
        assert_eq!(t.cluster.network().unwrap(), None);
        assert_eq!(apply(&t, &content(), 5), WriteResponse::Recorded);
        assert!(t.peers.current_for("a").unwrap().is_some());
        assert!(t.cluster.network().unwrap().is_some());
    }

    #[test]
    fn imported_logs_continue_after_the_source_head() {
        let t = target();
        apply(&t, &content(), 5);
        assert_eq!(t.peers.current_entry("a").unwrap().unwrap().0, 41);
        assert_eq!(t.peers.current_entry("b").unwrap().unwrap().0, 42);
        // The next write continues the numbering.
        assert_eq!(
            t.peers
                .register_applied(&origin("c", "10.60.0.3"), Some(6))
                .unwrap(),
            crate::store::Applied::Written(43)
        );
    }

    #[test]
    fn a_subscriber_with_an_old_cursor_receives_the_imported_registrations() {
        let t = target();
        apply(&t, &content(), 5);
        let seen = t.peers.store.revisions_after(40).unwrap();
        let revisions: Vec<u64> = seen.iter().map(|(r, _)| *r).collect();
        assert_eq!(revisions, [41, 42]);
    }

    #[test]
    fn an_unparsable_network_is_rejected_and_imports_nothing() {
        let t = target();
        let mut bad = content();
        bad.network = Some("nonsense".into());
        assert!(matches!(
            apply(&t, &bad, 5),
            WriteResponse::Rejected(Rejection::InvalidNetwork(_))
        ));
        assert_eq!(t.peers.current_for("a").unwrap(), None);
        assert_eq!(t.cluster.network().unwrap(), None);
    }
}
