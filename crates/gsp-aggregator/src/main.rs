//! `gsp-aggregator` binary — see `lib.rs` for the design pointer and slice
//! plan. Slice 6: `POST /ingest` + `GET /healthz`. `/fleet/*` reads (slice
//! 8), intent-verb fan-out (slice 9), and `--auth-token` (slice 10) aren't
//! built yet.

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

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router(AppState::new(store)));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "gsp-aggregator listening");
    axum::serve(listener, app).await?;

    Ok(())
}
