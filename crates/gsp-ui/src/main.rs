//! `gsp-ui` binary — see `lib.rs` for the design pointer and slice plan.
//! Slice 11b: session login/logout. Slice 11c: fleet reads + operational
//! verbs proxied to `gsp-aggregator`. Slice 11d: the browser WebSocket, fed
//! by a shared subscription to the aggregator's `/fleet/subscribe`. Slice
//! 11e: `gsp-controller`'s config API, proxied the same way. Slice 11f:
//! `--static-dir` serves the built React/Vite/TS frontend (`web/`) as a
//! fallback under every route the API doesn't claim — `gsp-ui` is the one
//! process, one port an operator's browser ever talks to.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::routing::get;
use axum::Router;
use clap::Parser;
use tower_http::services::ServeDir;
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

    /// `gsp-aggregator` base URL (e.g. "http://127.0.0.1:9902") that fleet
    /// reads and operational actions proxy to. Omitted: those routes return
    /// 503.
    #[arg(long)]
    aggregator_url: Option<String>,

    /// Bearer token this UI presents to `--aggregator-url`, if it requires
    /// one (its own `--auth-token`).
    #[arg(long)]
    aggregator_token: Option<String>,

    /// `gsp-controller` base URL (e.g. "http://127.0.0.1:9901") that
    /// structural-config actions proxy to. Omitted: those routes return 503.
    #[arg(long)]
    controller_url: Option<String>,

    /// Bearer token this UI presents to `--controller-url`, if it requires
    /// one (its own `--auth-token`).
    #[arg(long)]
    controller_token: Option<String>,

    /// Directory holding the built frontend (`web/`'s `npm run build`
    /// output — `make ui`). Default assumes the process runs from the repo
    /// root, matching every other `cargo run -p ...` example in this repo.
    /// A missing directory doesn't fail startup — requests for it just 404,
    /// same as running `gsp-ui` for its API alone (as every test in this
    /// crate does).
    #[arg(long, default_value = "crates/gsp-ui/web/dist")]
    static_dir: PathBuf,
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
    let mut state = AppState::new(args.ui_password);
    let mut feed_task = None;
    if let Some(aggregator_url) = args.aggregator_url {
        state = state.with_aggregator(aggregator_url.clone(), args.aggregator_token.clone());
        let feed = gsp_ui::fleet_feed::FleetFeed::new();
        feed_task = Some(tokio::spawn(gsp_ui::fleet_feed::run(
            aggregator_url,
            args.aggregator_token,
            feed.clone(),
        )));
        state = state.with_fleet_feed(feed);
    }
    if let Some(controller_url) = args.controller_url {
        state = state.with_controller(controller_url, args.controller_token);
    }
    tracing::info!(
        login_required,
        aggregator_configured = state.aggregator.is_some(),
        controller_configured = state.controller.is_some(),
        "gsp-ui starting"
    );

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router(state))
        .fallback_service(ServeDir::new(&args.static_dir));

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(listen = %args.listen, "gsp-ui listening");
    axum::serve(listener, app).await?;

    if let Some(task) = feed_task {
        task.abort();
    }
    Ok(())
}
