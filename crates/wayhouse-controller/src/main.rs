//! `wayhouse-controller` binary — see `lib.rs` for the design pointer and slice
//! plan. Slice 1: open the store, serve `/healthz` so the process is already
//! observable the same way `wayhouse` is. Slice 2: `POST`/`GET /config`. Slice 3:
//! `GET /config/subscribe`. Slice 5: revision history/diff/rollback +
//! `--auth-token`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use wayhouse_controller::adopt::AdoptState;
use wayhouse_controller::api::{self, AppState};
use wayhouse_controller::ha::{self, HaHandle, NodeId as HaNodeId};
use wayhouse_controller::intent::api::IntentState;
use wayhouse_controller::peers::api::PeersState;
use wayhouse_controller::proxy_peers::api::ProxyPeersState;
use wayhouse_controller::role::{Role, RoleHandle};
use wayhouse_controller::store::Store;

#[derive(Parser, Debug)]
#[command(
    name = "wayhouse-controller",
    version = wayhouse_http::LONG_VERSION,
    about = "Tier-1 config/intent controller for a wayhouse fleet (single standalone node — see docs/10)"
)]
struct Args {
    /// Directory for the embedded revision store (created if missing).
    #[arg(long, default_value = "wayhouse-controller-data")]
    data_dir: PathBuf,

    /// Address the controller's API listens on.
    #[arg(long, default_value = "127.0.0.1:9901")]
    listen: SocketAddr,

    /// Bearer token required on every /config* request, at least 16 bytes.
    /// Omit to leave the API open, which is only accepted on a loopback
    /// `--listen` (or with `--insecure-no-auth`). Not a full RBAC/identity
    /// story, just a shared secret (see `wayhouse_http::server`).
    #[arg(long)]
    auth_token: Option<String>,

    /// Bearer token `GET /metrics` accepts instead of `--auth-token`, at least
    /// 16 bytes, so a Prometheus scraper need not hold the admin token. It
    /// unlocks only `/metrics`. Omitted: `/metrics` is gated by `--auth-token`
    /// like the rest of the API.
    #[arg(long)]
    metrics_token: Option<String>,

    /// `standalone` (default) accepts writes directly. `slave` never does —
    /// it relays a parent controller's revision stream instead; requires
    /// `--parent-url` (phase 12 slice 1, see `docs/10` "Fleet topology").
    #[arg(long, value_enum, default_value_t = Role::Standalone)]
    role: Role,

    /// Parent controller's base URL — required when `--role slave`.
    #[arg(long)]
    parent_url: Option<String>,

    /// Bearer token this tier presents to its parent's `/config*` API.
    #[arg(long)]
    parent_token: Option<String>,

    /// This node's own ID within its tier's Raft group. Required with
    /// `--ha-peers` (phase 12 slice 6, see `docs/10` "Intra-tier HA
    /// (design)").
    #[arg(long)]
    ha_node_id: Option<HaNodeId>,

    /// The tier's full replica set, `id=host:port` pairs separated by
    /// commas (e.g. `1=127.0.0.1:9901,2=127.0.0.1:9911,3=127.0.0.1:9921`),
    /// identical on every replica. `host:port` is plain HTTP; an entry may
    /// instead be a base URL, `id=https://host[:port]` (e.g. a TLS
    /// terminator in front of that replica; trusts `--ca-file`). Read only
    /// when the cluster is first bootstrapped. Setting this turns on HA: writes
    /// propose a Raft entry instead of writing the store directly, and a
    /// non-leader replica transparently forwards a write to the current
    /// leader. Requires `--ha-node-id`. Combines with `--role slave`: the
    /// Raft leader alone relays the parent's revisions into the group, and
    /// the relay cursor is replicated, so a new leader resumes where the old
    /// one left off.
    #[arg(long, value_delimiter = ',')]
    ha_peers: Vec<String>,

    /// Start as a node that never bootstraps a Raft cluster and waits to be
    /// added (`POST /admin/ha/members` on a running cluster). Use it for a
    /// new or replacement node; mutually exclusive with `--ha-peers`. Needs
    /// `--ha-node-id` and `--ha-token`; restart a joined node with it again.
    #[arg(long)]
    ha_join: bool,

    /// Peer-only shared secret gating `/raft/*` — a separate secret from
    /// `--auth-token` (client-facing), matching every other peer-to-peer
    /// credential in this fleet.
    #[arg(long)]
    ha_token: Option<String>,

    /// Allow a non-loopback `--listen` with no `--auth-token`. Without it the
    /// controller refuses that combination at startup. Only for deployments
    /// where the network boundary is the sole access control.
    #[arg(long)]
    insecure_no_auth: bool,

    /// Build a Raft snapshot (and let the log be purged) every this many
    /// log entries. Test-only: fleet tests lower it to exercise catch-up by
    /// snapshot.
    /// Which node's pre-HA registry data seeds a freshly bootstrapped HA
    /// cluster: a node id, or `none` to import nothing. Needed only when more
    /// than one node holds data from before HA (the controller refuses to
    /// guess); give every node the same value, like `--ha-peers`.
    #[arg(long, value_name = "ID|none")]
    ha_import_source: Option<String>,

    #[arg(long, hide = true, default_value_t = ha::SNAPSHOT_AFTER)]
    ha_snapshot_after: u64,

    /// Network tunnel addresses are allocated from: IPv6 `/64` to `/120`, e.g.
    /// `fd49:89c1:4b5e:60::/64`, or IPv4 `/16` to `/30`, e.g. `10.60.0.0/16`.
    /// Omit for pin-only mode: requested addresses are checked for uniqueness
    /// but nothing is allocated. Cannot be combined with `--ha-peers`.
    #[arg(long)]
    tunnel_network: Option<String>,

    /// An allocated tunnel address not re-registered for this long is flagged
    /// `stale` in `GET /tunnel/addresses` and in a daily log warning. Units:
    /// s, m, h, d. `0` disables it.
    #[arg(long, default_value = "14d")]
    tunnel_stale_after: String,

    /// Release a tunnel address (and its registration) whose owner has not
    /// re-registered for this long, as if it had been `DELETE`d. Units: s, m,
    /// h, d; at least 2h. `0` never expires: only an explicit release frees
    /// an address.
    #[arg(long, default_value = "0")]
    tunnel_lease_ttl: String,

    /// Start even though stored tunnel addresses fall outside
    /// `--tunnel-network`, and move each such peer into the network at its
    /// next registration (docs/superpowers/specs/2026-10-03-ipv6-tunnel-design.md).
    /// Re-addressing interrupts the peer's traffic until it restarts.
    #[arg(long)]
    tunnel_readdress: bool,

    /// PEM file of extra CA certificates to trust for outbound HTTPS, in
    /// addition to the built-in Mozilla roots.
    #[arg(long)]
    ca_file: Option<PathBuf>,

    /// Serve the plugin API (`/plugins`): upload, approve and manage WASM plugin
    /// installs (docs/plugins.md). Off by default. With `--role slave` the routes answer
    /// `501`. Installs are stored and enabled plugins that declared a timer are ticked;
    /// under `--ha-peers` installs and state are replicated and only the Raft leader
    /// ticks. Module bytes are not replicated yet. Installs made on a standalone
    /// controller are not carried over when `--ha-peers` is turned on: install again.
    #[arg(long)]
    plugins: bool,

    #[command(flatten)]
    tls: wayhouse_http::tls::TlsArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.role == Role::Slave && args.parent_url.is_none() {
        anyhow::bail!("--role slave requires --parent-url");
    }
    ha::check_flags(!args.ha_peers.is_empty(), args.ha_join, args.ha_node_id)
        .map_err(|e| anyhow::anyhow!(e))?;
    ha::check_ha_token(
        !args.ha_peers.is_empty() || args.ha_join,
        args.ha_token.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    wayhouse_http::policy::check_optional_secret("--auth-token", args.auth_token.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;
    wayhouse_http::policy::check_optional_secret("--metrics-token", args.metrics_token.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;
    let (tunnel_network, stale_after) = wayhouse_controller::addresses::resolve_flags(
        args.tunnel_network.as_deref(),
        &args.tunnel_stale_after,
        !args.ha_peers.is_empty() || args.ha_join,
        args.tunnel_readdress,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    let lease_ttl = wayhouse_controller::addresses::parse_duration(&args.tunnel_lease_ttl)
        .map_err(|e| format!("--tunnel-lease-ttl: {e}"))
        .and_then(|ttl| wayhouse_controller::lease::check_ttl(ttl).map(|()| ttl))
        .map_err(|e| anyhow::anyhow!(e))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("WAYHOUSE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let prometheus =
        wayhouse_http::metrics::install("wayhouse-controller", env!("CARGO_PKG_VERSION"))?;
    wayhouse_http::policy::check_exposure(
        "wayhouse-controller",
        args.listen,
        args.auth_token.is_some(),
        args.insecure_no_auth,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(path) = &args.ca_file {
        let certs = wayhouse_http::init_ca_file(path)?;
        tracing::info!(certs, path = %path.display(), "trusting extra CAs from --ca-file");
    }
    // Load (and validate) the serving certificate before anything else starts.
    let tls_cert = args.tls.load()?;
    let ha_enabled = !args.ha_peers.is_empty() || args.ha_join;
    let import_policy = match args.ha_import_source.as_deref() {
        None => ha::init::ImportPolicy::Auto,
        Some("none") => ha::init::ImportPolicy::None,
        Some(id) => ha::init::ImportPolicy::Source(id.parse().map_err(|_| {
            anyhow::anyhow!("--ha-import-source takes a node id or `none`, not {id:?}")
        })?),
    };
    // Registry data from before HA is set aside (never deleted) before any
    // database opens; the cluster's leader imports one node's copy.
    let pre_ha = if ha_enabled {
        ha::import::LocalPreHa {
            summary: ha::import::set_aside_pre_ha(&args.data_dir)?,
            data_dir: args.data_dir.clone(),
            network: tunnel_network,
        }
    } else {
        ha::import::LocalPreHa::default()
    };

    let store = Arc::new(
        Store::open(&args.data_dir)
            .map_err(|e| anyhow::anyhow!("opening store at {:?}: {e}", args.data_dir))?,
    );
    let current_revision = store
        .current_revision()
        .map_err(|e| anyhow::anyhow!("reading store state: {e}"))?;
    tracing::info!(
        data_dir = %args.data_dir.display(),
        ?current_revision,
        auth = args.auth_token.is_some(),
        role = %args.role,
        "controller store opened"
    );

    // A separate sled database in its own subdirectory — the intent log
    // (phase 12 slice 3) is content-wise unrelated to config revisions, and
    // keeping it out of the existing `data_dir` root leaves every
    // already-deployed config store's on-disk layout untouched.
    let intent_dir = args.data_dir.join("intent");
    let intent_store = Arc::new(
        Store::open(&intent_dir)
            .map_err(|e| anyhow::anyhow!("opening intent store at {intent_dir:?}: {e}"))?,
    );
    tracing::info!(
        intent_dir = %intent_dir.display(),
        "intent store opened"
    );

    // A third separate sled database (phase 14 slice 2) — the backend-peers
    // registry is content-wise unrelated to both config and intent.
    let peers_dir = args.data_dir.join("peers");
    let peers_store = Arc::new(
        Store::open(&peers_dir)
            .map_err(|e| anyhow::anyhow!("opening peers store at {peers_dir:?}: {e}"))?,
    );
    tracing::info!(
        peers_dir = %peers_dir.display(),
        "backend-peers store opened"
    );
    // The address book (spec: Controller/State) — its own sled database like
    // every other registry, shared by both peer registries below.
    let addresses_dir = args.data_dir.join("tunnel-addresses");
    // Under HA the book carries no network of its own: the state machine
    // applies every claim against the network the cluster recorded, so a
    // node's flag never decides an address (see `ha::init`).
    let book_network = if ha_enabled { None } else { tunnel_network };
    let book = wayhouse_controller::addresses::AddressBook::open(&addresses_dir, book_network)
        .map_err(|e| anyhow::anyhow!("opening the address book at {addresses_dir:?}: {e}"))?
        .with_readdress(args.tunnel_readdress);
    // Before any listener binds: a changed network is refused unless the
    // operator opted in, so a mistyped flag never re-addresses anyone.
    // (Not under HA: `--tunnel-readdress` is refused there, and the recorded
    // network, not this flag, decides.)
    let moving = wayhouse_controller::addresses::check_network_change(&book, args.tunnel_readdress)
        .map_err(|e| anyhow::anyhow!(e))?;
    if !moving.is_empty() {
        tracing::warn!(
            "--tunnel-readdress: {} stored tunnel address(es) are outside the network and \
             move at their owner's next registration:\n{}",
            moving.len(),
            wayhouse_controller::addresses::summarize(&moving)
        );
    }
    let book = Arc::new(book);
    match tunnel_network {
        Some(n) => tracing::info!(network = %n, "tunnel address allocation enabled"),
        None => tracing::info!("no --tunnel-network: pin-only tunnel addresses (no allocation)"),
    }
    let mut peers_state = PeersState::new(peers_store, args.auth_token.clone(), book.clone());

    // A fourth separate sled database (phase 14 slice 7) — the proxy-peers
    // registry, the mirror image of the backend-peers one above (see
    // `wayhouse_controller::proxy_peers`'s module doc for why it's a separate
    // resource rather than folded into `peers`).
    let proxy_peers_dir = args.data_dir.join("proxy-peers");
    let proxy_peers_store =
        Arc::new(Store::open(&proxy_peers_dir).map_err(|e| {
            anyhow::anyhow!("opening proxy-peers store at {proxy_peers_dir:?}: {e}")
        })?);
    tracing::info!(
        proxy_peers_dir = %proxy_peers_dir.display(),
        "proxy-peers store opened"
    );
    let mut proxy_peers_state =
        ProxyPeersState::new(proxy_peers_store, args.auth_token.clone(), book.clone());
    let mut addresses_state = wayhouse_controller::addresses::api::AddressesState::new(
        book.clone(),
        args.auth_token.clone(),
        stale_after,
    );

    // Shared, mutable across `AppState` and `IntentState` — `adopt` flips
    // this one cell and both write gates see it instantly (see
    // `role::RoleHandle`'s doc for why a plain `Role` field per state
    // wouldn't work once adoption exists).
    let role_handle = RoleHandle::new(args.role);
    // POST /config refuses a document newer than the oldest live proxy
    // understands; the clone reads the same trees `with_ha` below keeps using.
    let schema_source = proxy_peers_state.clone();
    let mut config_state = AppState::try_new(store, args.auth_token.clone(), role_handle.clone())?
        .with_schema_floor(Arc::new(move || schema_source.min_live_config_schema()));
    let mut intent_state_val =
        IntentState::try_new(intent_store, role_handle.clone(), args.auth_token.clone())?;

    // Intra-tier HA (phase 12 slice 6): one Raft group per tier replicating
    // both the config and intent logs together — see `wayhouse_controller::ha`'s
    // module doc. `None` (no `--ha-peers`, the default `replicas: 1` shape)
    // leaves every write on the exact same direct-to-`Store` path phase
    // 10+11 shipped.
    let ha_handle: Option<(
        Arc<HaHandle>,
        Arc<ha::cluster_state::ClusterState>,
        wayhouse_controller::plugins::PluginStore,
    )> = if !ha_enabled {
        None
    } else {
        let node_id = args.ha_node_id.expect("checked above");
        let peers = ha::peers::parse_peers(&args.ha_peers)?;
        // This node's own entry is never dialled.
        for node in peers
            .iter()
            .filter(|(id, _)| **id != node_id)
            .map(|(_, n)| n)
        {
            ha::peers::warn_if_plain_remote(&node.addr);
        }
        // `--ha-join` leaves `peers` empty: this node never initializes.
        let ha_token: Option<Arc<str>> = args.ha_token.clone().map(Arc::from);

        let ha_dir = args.data_dir.join("ha");
        let db = sled::open(&ha_dir)
            .map_err(|e| anyhow::anyhow!("opening HA store at {ha_dir:?}: {e}"))?;
        let log_store = ha::log_store::LogStore::open(&db)
            .map_err(|e| anyhow::anyhow!("opening raft log at {ha_dir:?}: {e}"))?;
        let state_machine = Arc::new(
            ha::state_machine::StateMachineStore::open(
                &db,
                Arc::new(config_state.clone()),
                Arc::new(intent_state_val.clone()),
                ha::state_machine::Registries {
                    peers: Arc::new(peers_state.clone()),
                    proxy_peers: Arc::new(proxy_peers_state.clone()),
                    book: book.clone(),
                },
            )
            .map_err(|e| anyhow::anyhow!("opening raft state machine meta at {ha_dir:?}: {e}"))?
            .with_pre_ha_notice(node_id, args.data_dir.clone()),
        );

        let cluster = state_machine.cluster().clone();
        let plugin_store = state_machine.plugins().clone();
        let network = ha::network::Network::new(ha_token.clone());
        let raft_config = Arc::new(
            ha::raft_config(args.ha_snapshot_after)
                .validate()
                .map_err(|e| anyhow::anyhow!("invalid raft config: {e}"))?,
        );
        let raft = openraft::Raft::new(node_id, raft_config, network, log_store, state_machine)
            .await
            .map_err(|e| anyhow::anyhow!("starting raft: {e}"))?;

        // Bootstrap: every replica in a fresh cluster calls `initialize`
        // with the identical static peer set from `--ha-peers`; `openraft`
        // requires an empty log for it to succeed, so at most one call
        // actually wins the race (the rest error harmlessly once any node's
        // log has content — logged at `debug`, not a real failure). A
        // brief delay gives every peer's HTTP server (this one included)
        // time to come up first.
        // A `--ha-join` node never initializes: it waits to be added.
        if !args.ha_join {
            let bootstrap_raft = raft.clone();
            let members = peers.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                match bootstrap_raft.initialize(members).await {
                    Ok(()) => tracing::info!("raft cluster initialized"),
                    Err(e) => tracing::debug!(
                        error = %e,
                        "raft initialize was a no-op (already initialized by this node or a \
                         peer, which is the expected outcome for every node but the one that \
                         won the race)"
                    ),
                }
            });
        }

        let handle = Arc::new(HaHandle {
            raft,
            node_id,
            ha_token,
            forward: ha::client::forward_client(ha::client::FORWARD_TIMEOUT),
            pre_ha,
        });
        config_state = config_state.with_ha(Some(handle.clone()));
        intent_state_val = intent_state_val.with_ha(Some(handle.clone()));
        if args.ha_join {
            tracing::info!(
                node_id,
                "intra-tier HA enabled; waiting to be added to a cluster"
            );
        } else {
            tracing::info!(node_id, peers = ?peers.keys().collect::<Vec<_>>(), "intra-tier HA enabled");
        }
        Some((handle, cluster, plugin_store))
    };

    // Under HA the registries propose through Raft instead of writing: the
    // state machine's own copies (handed to it above) stay the writers.
    if let Some((handle, cluster, _)) = &ha_handle {
        let registry_ha = wayhouse_controller::registry::RegistryHa {
            handle: handle.clone(),
            cluster: cluster.clone(),
            local_network: tunnel_network,
        };
        peers_state = peers_state.with_ha(Some(registry_ha.clone()));
        proxy_peers_state = proxy_peers_state.with_ha(Some(registry_ha));
        addresses_state = addresses_state.with_cluster(cluster.clone());
        tokio::spawn(ha::init::initialize_registries(
            handle.clone(),
            cluster.clone(),
            tunnel_network,
            import_policy,
        ));
    }
    let leader_handle = ha_handle.as_ref().map(|(h, _, _)| h.clone());
    let lease_leader = leader_handle.clone();
    tokio::spawn(wayhouse_controller::lease::lease_loop(
        book.clone(),
        peers_state.clone(),
        proxy_peers_state.clone(),
        lease_ttl,
        move || {
            lease_leader
                .as_ref()
                .is_none_or(|h| h.raft.metrics().borrow().current_leader == Some(h.node_id))
        },
    ));
    tokio::spawn(wayhouse_controller::addresses::api::stale_warning_loop(
        book.clone(),
        stale_after,
        move || {
            leader_handle
                .as_ref()
                .is_none_or(|h| h.raft.metrics().borrow().current_leader == Some(h.node_id))
        },
    ));

    let state = Arc::new(config_state);
    let intent_state = Arc::new(intent_state_val);

    if args.role == Role::Slave {
        let parent_url = args.parent_url.expect("checked above");
        // Without HA, seed from the parent's current revision before
        // serving, same as `wayhouse --controller`'s initial `fetch_current`
        // — a slave starting cold shouldn't serve `404` for however long the
        // first subscribe catch-up takes if the parent already has
        // something. (Under HA the leader's relay seeds through Raft once it
        // is elected.) The intent log has no equivalent seed (see
        // `intent::relay`'s doc) — its relay just subscribes from its stored
        // cursor.
        if state.ha.is_none() {
            if let Err(e) = wayhouse_controller::parent_client::seed_if_cold(
                &parent_url,
                args.parent_token.as_deref(),
                &state,
            )
            .await
            {
                tracing::warn!(
                    error = %e, parent = %parent_url,
                    "could not seed from the parent controller at startup; will keep retrying via subscribe"
                );
            }
        }

        tokio::spawn(wayhouse_controller::parent_client::run(
            parent_url.clone(),
            args.parent_token.clone(),
            state.clone(),
        ));
        tokio::spawn(wayhouse_controller::intent::relay::run(
            parent_url,
            args.parent_token,
            intent_state.clone(),
        ));
    }

    let admin_token: Option<Arc<str>> = args.auth_token.as_deref().map(Arc::from);
    let adopt_state = AdoptState {
        role: role_handle,
        config: state.clone(),
        intent: intent_state.clone(),
        auth_token: args.auth_token.map(Arc::from),
    };

    let mut app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(wayhouse_http::metrics::router(
            prometheus,
            wayhouse_http::server::BearerAuth::new(
                args.metrics_token.as_deref().or(admin_token.as_deref()),
            ),
        ))
        .merge(api::router((*state).clone()))
        .merge(wayhouse_controller::intent::api::router(
            (*intent_state).clone(),
        ))
        .merge(wayhouse_controller::peers::api::router(peers_state))
        .merge(wayhouse_controller::proxy_peers::api::router(
            proxy_peers_state,
        ))
        .merge(wayhouse_controller::addresses::api::router(addresses_state))
        .merge(wayhouse_controller::adopt::router(adopt_state));
    app = app.merge(
        match wayhouse_controller::plugins::availability(args.plugins, args.role == Role::Slave) {
            Ok(()) => {
                let host = Arc::new(wayhouse_plugin_host::PluginHost::new(
                    wayhouse_plugin_host::Limits::default(),
                )?);
                let pool = Arc::new(wayhouse_plugin_host::CompilePool::new(
                    host,
                    2,
                    8,
                    Duration::from_secs(30),
                )?);
                // Under HA the state machine's store is the one every replica applies
                // into; otherwise the plugin trees sit next to the config revisions.
                let (store, ha) = match &ha_handle {
                    Some((handle, _, store)) => (store.clone(), Some(handle.clone())),
                    None => (
                        wayhouse_controller::plugins::PluginStore::open(state.store.db())?,
                        None,
                    ),
                };
                let runner = match &ha {
                    Some(handle) => wayhouse_controller::plugins::runner::Runner::new_ha(
                        store.clone(),
                        pool.clone(),
                        handle.clone(),
                    ),
                    None => wayhouse_controller::plugins::runner::Runner::new(
                        store.clone(),
                        pool.clone(),
                    ),
                };
                match store.sweep_blobs() {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(removed = n, "removed plugin modules no install uses"),
                    Err(e) => tracing::warn!(error = %e, "could not sweep orphaned plugin modules"),
                }
                // Detached on purpose: they run for the life of the process.
                drop(runner.clone().spawn());
                let mut plugin_routes = wayhouse_controller::plugins::api::router(
                    wayhouse_controller::plugins::api::PluginsState {
                        store: store.clone(),
                        pool,
                        runner,
                        auth_token: admin_token.clone(),
                        ha: ha.clone(),
                    },
                );
                if let Some(handle) = &ha {
                    // Replicas hand each other the module bytes the Raft log leaves out.
                    plugin_routes = plugin_routes.merge(
                        wayhouse_controller::plugins::peer::router(handle, store.clone()),
                    );
                    drop(wayhouse_controller::plugins::peer::spawn_sync(
                        store,
                        handle.clone(),
                    ));
                }
                plugin_routes
            }
            Err(reason) => wayhouse_controller::plugins::api::disabled_router(reason),
        },
    );
    if let Some((handle, _, _)) = ha_handle {
        app = app
            .merge(ha::members::router(handle.clone(), admin_token))
            .merge(ha::routes::router(handle));
    }

    wayhouse_http::tls::serve(
        args.listen,
        app,
        tls_cert,
        args.tls.limits(),
        "wayhouse-controller",
    )
    .await?;

    Ok(())
}
