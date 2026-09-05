//! `gsp-controller` binary — see `lib.rs` for the design pointer and slice
//! plan. Slice 1: open the store, serve `/healthz` so the process is already
//! observable the same way `gsp` is. Slice 2: `POST`/`GET /config`. Slice 3:
//! `GET /config/subscribe`. Slice 5: revision history/diff/rollback +
//! `--auth-token`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use gsp_controller::api::{self, AppState};
use gsp_controller::role::Role;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.role == Role::Slave && args.parent_url.is_none() {
        anyhow::bail!("--role slave requires --parent-url");
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

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

    let state = Arc::new(AppState::new(store, args.auth_token, args.role));

    if args.role == Role::Slave {
        let parent_url = args.parent_url.expect("checked above");
        // Seed from the parent's current revision before serving, same as
        // `gsp --controller`'s initial `fetch_current` — a slave starting
        // cold shouldn't serve `404` for however long the first subscribe
        // catch-up takes if the parent already has something.
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

        let relay_state = state.clone();
        tokio::spawn(gsp_controller::parent_client::run(
            parent_url,
            args.parent_token,
            initial_cursor,
            relay_state,
        ));
    }

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router((*state).clone()));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "gsp-controller listening");
    axum::serve(listener, app).await?;

    Ok(())
}
