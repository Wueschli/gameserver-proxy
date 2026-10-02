//! `gsp-controller` binary — see `lib.rs` for the design pointer and slice
//! plan. Slice 1: open the store, serve `/healthz` so the process is already
//! observable the same way `gsp` is. Slice 2: `POST`/`GET /config`. Slice 3:
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

use gsp_controller::adopt::AdoptState;
use gsp_controller::api::{self, AppState};
use gsp_controller::ha::{self, HaHandle, NodeId as HaNodeId};
use gsp_controller::intent::api::IntentState;
use gsp_controller::peers::api::PeersState;
use gsp_controller::proxy_peers::api::ProxyPeersState;
use gsp_controller::role::{Role, RoleHandle};
use gsp_controller::store::Store;

#[derive(Parser, Debug)]
#[command(
    name = "gsp-controller",
    version,
    about = "Tier-1 config/intent controller for a gsp fleet (single standalone node — see docs/10)"
)]
struct Args {
    /// Directory for the embedded revision store (created if missing).
    #[arg(long, default_value = "gsp-controller-data")]
    data_dir: PathBuf,

    /// Address the controller's API listens on.
    #[arg(long, default_value = "127.0.0.1:9901")]
    listen: SocketAddr,

    /// Bearer token required on every /config* request. Omit to leave the
    /// API open — network-boundary-only auth, the same posture `gsp`'s own
    /// admin API has today. Not a full RBAC/identity story, just a shared
    /// secret (see `crate::auth`).
    #[arg(long)]
    auth_token: Option<String>,

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
    /// leader. Requires `--ha-node-id`. **Mutually exclusive with `--role
    /// slave`** in this slice — combining HA with the slave role needs the
    /// upward relay to run leader-only with a replicated cursor, designed
    /// in `docs/10` but not yet built (see `crate::ha`'s module doc).
    #[arg(long, value_delimiter = ',')]
    ha_peers: Vec<String>,

    /// Peer-only shared secret gating `/raft/*` — a separate secret from
    /// `--auth-token` (client-facing), matching every other peer-to-peer
    /// credential in this fleet.
    #[arg(long)]
    ha_token: Option<String>,

    /// IPv4 network tunnel addresses are allocated from, e.g. `10.60.0.0/16`.
    /// Omit for pin-only mode: requested addresses are checked for uniqueness
    /// but nothing is allocated. Cannot be combined with `--ha-peers`.
    #[arg(long)]
    tunnel_network: Option<String>,

    /// An allocated tunnel address not re-registered for this long is flagged
    /// `stale` in `GET /tunnel/addresses` and in a daily log warning. Units:
    /// s, m, h, d. `0` disables it.
    #[arg(long, default_value = "14d")]
    tunnel_stale_after: String,

    /// PEM file of extra CA certificates to trust for outbound HTTPS, in
    /// addition to the built-in Mozilla roots.
    #[arg(long)]
    ca_file: Option<PathBuf>,

    #[command(flatten)]
    tls: gsp_http::tls::TlsArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.role == Role::Slave && args.parent_url.is_none() {
        anyhow::bail!("--role slave requires --parent-url");
    }
    if !args.ha_peers.is_empty() && args.ha_node_id.is_none() {
        anyhow::bail!("--ha-peers requires --ha-node-id");
    }
    if !args.ha_peers.is_empty() && args.role == Role::Slave {
        // Scope cut for this slice — see `gsp_controller::ha`'s module doc.
        anyhow::bail!(
            "--ha-peers cannot be combined with --role slave yet: the upward relay needs to \
             run leader-only with a replicated cursor, which is designed (docs/10) but not \
             built in this slice"
        );
    }
    let (tunnel_network, stale_after) = gsp_controller::addresses::resolve_flags(
        args.tunnel_network.as_deref(),
        &args.tunnel_stale_after,
        !args.ha_peers.is_empty(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    if let Some(path) = &args.ca_file {
        let certs = gsp_http::init_ca_file(path)?;
        tracing::info!(certs, path = %path.display(), "trusting extra CAs from --ca-file");
    }
    // Load (and validate) the serving certificate before anything else starts.
    let tls_cert = args.tls.load()?;
    if !args.ha_peers.is_empty() {
        // resolve_flags refuses --tunnel-network under HA, so this is pin-only mode.
        tracing::warn!(
            "tunnel address uniqueness under --ha-peers is enforced per controller node only, \
             because the peer registries are not replicated; point all origins and proxies at \
             one node"
        );
    }

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
    let book = Arc::new(
        gsp_controller::addresses::AddressBook::open(&addresses_dir, tunnel_network)
            .map_err(|e| anyhow::anyhow!("opening the address book at {addresses_dir:?}: {e}"))?,
    );
    match tunnel_network {
        Some(n) => tracing::info!(network = %n, "tunnel address allocation enabled"),
        None => tracing::info!("no --tunnel-network: pin-only tunnel addresses (no allocation)"),
    }
    let peers_state = PeersState::new(peers_store, args.auth_token.clone(), book.clone());

    // A fourth separate sled database (phase 14 slice 7) — the proxy-peers
    // registry, the mirror image of the backend-peers one above (see
    // `gsp_controller::proxy_peers`'s module doc for why it's a separate
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
    let proxy_peers_state =
        ProxyPeersState::new(proxy_peers_store, args.auth_token.clone(), book.clone());
    let addresses_state = gsp_controller::addresses::api::AddressesState::new(
        book.clone(),
        args.auth_token.clone(),
        stale_after,
    );
    tokio::spawn(gsp_controller::addresses::api::stale_warning_loop(
        book.clone(),
        stale_after,
    ));

    // Shared, mutable across `AppState` and `IntentState` — `adopt` flips
    // this one cell and both write gates see it instantly (see
    // `role::RoleHandle`'s doc for why a plain `Role` field per state
    // wouldn't work once adoption exists).
    let role_handle = RoleHandle::new(args.role);
    let mut config_state = AppState::new(store, args.auth_token.clone(), role_handle.clone());
    let mut intent_state_val =
        IntentState::new(intent_store, role_handle.clone(), args.auth_token.clone());

    // Intra-tier HA (phase 12 slice 6): one Raft group per tier replicating
    // both the config and intent logs together — see `gsp_controller::ha`'s
    // module doc. `None` (no `--ha-peers`, the default `replicas: 1` shape)
    // leaves every write on the exact same direct-to-`Store` path phase
    // 10+11 shipped.
    let ha_handle: Option<Arc<HaHandle>> = if args.ha_peers.is_empty() {
        None
    } else {
        let node_id = args.ha_node_id.expect("checked above");
        let peers = ha::peers::parse_peers(&args.ha_peers)?;
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
            )
            .map_err(|e| anyhow::anyhow!("opening raft state machine meta at {ha_dir:?}: {e}"))?,
        );

        let network = ha::network::Network::new(ha_token.clone());
        // Relaxed from the library defaults (150/300/50ms) — this is a
        // control-plane group on plain HTTP over `reqwest`, not a
        // low-latency data-path link; a wider election window trades a
        // slightly slower failover for fewer spurious elections under
        // ordinary scheduling/network jitter in a test or a loaded host.
        let raft_config = Arc::new(
            openraft::Config {
                heartbeat_interval: 250,
                election_timeout_min: 800,
                election_timeout_max: 1500,
                ..Default::default()
            }
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
        let bootstrap_raft = raft.clone();
        let members = peers.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            match bootstrap_raft.initialize(members).await {
                Ok(()) => tracing::info!("raft cluster initialized"),
                Err(e) => tracing::debug!(
                    error = %e,
                    "raft initialize was a no-op (already initialized by this node or a peer, \
                     which is the expected outcome for every node but the one that won the race)"
                ),
            }
        });

        let handle = Arc::new(HaHandle {
            raft,
            node_id,
            ha_token,
        });
        config_state = config_state.with_ha(Some(handle.clone()));
        intent_state_val = intent_state_val.with_ha(Some(handle.clone()));
        tracing::info!(node_id, peers = ?peers.keys().collect::<Vec<_>>(), "intra-tier HA enabled");
        Some(handle)
    };

    let state = Arc::new(config_state);
    let intent_state = Arc::new(intent_state_val);

    if args.role == Role::Slave {
        let parent_url = args.parent_url.expect("checked above");
        // Seed from the parent's current revision before serving, same as
        // `gsp --controller`'s initial `fetch_current` — a slave starting
        // cold shouldn't serve `404` for however long the first subscribe
        // catch-up takes if the parent already has something. The intent
        // log has no equivalent seed (see `intent::relay`'s doc) — its
        // relay just subscribes from `since=0` directly.
        let mut initial_cursor = 0u64;
        match gsp_controller::parent_client::fetch_initial(
            &parent_url,
            args.parent_token.as_deref(),
        )
        .await
        {
            Ok(Some((revision, config))) => {
                initial_cursor = revision;
                match state.apply_revision(config.into_bytes()) {
                    Ok(local_revision) => tracing::info!(
                        parent_revision = revision,
                        local_revision,
                        "seeded initial config from parent controller"
                    ),
                    Err(e) => tracing::error!(error = %e, "failed to store initial parent config"),
                }
            }
            Ok(None) => tracing::info!(
                parent = %parent_url,
                "parent controller has no config yet; waiting on subscribe"
            ),
            Err(e) => tracing::warn!(
                error = %e, parent = %parent_url,
                "could not reach parent controller at startup; will keep retrying via subscribe"
            ),
        }

        tokio::spawn(gsp_controller::parent_client::run(
            parent_url.clone(),
            args.parent_token.clone(),
            initial_cursor,
            state.clone(),
        ));
        tokio::spawn(gsp_controller::intent::relay::run(
            parent_url,
            args.parent_token,
            intent_state.clone(),
        ));
    }

    let adopt_state = AdoptState {
        role: role_handle,
        config: state.clone(),
        intent: intent_state.clone(),
        auth_token: args.auth_token.map(Arc::from),
    };

    let mut app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router((*state).clone()))
        .merge(gsp_controller::intent::api::router((*intent_state).clone()))
        .merge(gsp_controller::peers::api::router(peers_state))
        .merge(gsp_controller::proxy_peers::api::router(proxy_peers_state))
        .merge(gsp_controller::addresses::api::router(addresses_state))
        .merge(gsp_controller::adopt::router(adopt_state));
    if let Some(handle) = ha_handle {
        app = app.merge(ha::routes::router(handle));
    }

    gsp_http::tls::serve(args.listen, app, tls_cert, "gsp-controller").await?;

    Ok(())
}
