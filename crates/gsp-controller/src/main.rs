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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

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
        "controller store opened"
    );

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router(AppState::new(store, args.auth_token)));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "gsp-controller listening");
    axum::serve(listener, app).await?;

    Ok(())
}
