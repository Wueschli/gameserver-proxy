//! `gsp-ui` binary — see `lib.rs` for the design pointer and slice plan.
//! Slice 11b: session login/logout only. Proxying to `gsp-aggregator` /
//! `gsp-controller` (11c/11e), the WebSocket bridge (11d), and the frontend
//! itself (11f) aren't built yet.

use std::net::SocketAddr;

use axum::routing::get;
use axum::Router;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use gsp_ui::api::{self, AppState};

#[derive(Parser, Debug)]
#[command(
    name = "gsp-ui",
    version,
    about = "Operator dashboard BFF for a gsp fleet (docs/10 \"The admin GUI\")"
)]
struct Args {
    /// Address this UI's own HTTP server listens on.
    #[arg(long, default_value = "127.0.0.1:9903")]
    listen: SocketAddr,

    /// Password required to log in. Omit to leave the UI open (no login
    /// required) — network-boundary-only auth, same posture every other
    /// optional-auth surface in this fleet has.
    #[arg(long)]
    ui_password: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let login_required = args.ui_password.is_some();
    let state = AppState::new(args.ui_password);
    tracing::info!(login_required, "gsp-ui starting");

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router(state));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "gsp-ui listening");
    axum::serve(listener, app).await?;

    Ok(())
}
