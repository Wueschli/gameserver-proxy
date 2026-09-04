//! `gsp-aggregator` binary — see `lib.rs` for the design pointer and slice
//! plan. `POST /ingest`, `GET /fleet/*`, intent-verb fan-out
//! (`crate::fanout`), and `--auth-token`/`--instance-token` (slice 10) are
//! all built; `GET /healthz` stays unauthenticated, wired up separately from
//! `api::router`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use gsp_aggregator::api::{self, AppState};
use gsp_aggregator::ingest::IngestStore;

#[derive(Parser, Debug)]
#[command(
    name = "gsp-aggregator",
    version,
    about = "Fleet read/operational-verb aggregator for a gsp fleet (single tier — see docs/10)"
)]
struct Args {
    /// Address the aggregator's API listens on.
    #[arg(long, default_value = "127.0.0.1:9902")]
    listen: SocketAddr,

    /// Bearer token every request except /healthz must present. Omit to
    /// leave this aggregator's own API open (network-boundary-only auth).
    #[arg(long)]
    auth_token: Option<String>,

    /// Bearer token this aggregator presents when fanning intent verbs out
    /// to each instance's admin API (`settings.admin.auth_token` on `gsp`).
    /// A separate secret from `--auth-token`: one gates calls into this
    /// aggregator, this one is what it presents going out.
    #[arg(long)]
    instance_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // No data-dir, no persistence — the store is deliberately in-memory
    // only (see the "stateless and ephemeral by design" note in lib.rs).
    let store = Arc::new(IngestStore::new());
    let state = AppState::new(store)
        .with_auth_token(args.auth_token.clone())
        .with_instance_token(args.instance_token);

    tracing::info!(
        auth = args.auth_token.is_some(),
        "aggregator store initialized"
    );

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router(state));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "gsp-aggregator listening");
    axum::serve(listener, app).await?;

    Ok(())
}
