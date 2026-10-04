//! `gsp-ui` binary — see `lib.rs` for the design pointer and slice plan.
//! Slice 11b: session login/logout. Slice 11c: fleet reads + operational
//! verbs proxied to `gsp-aggregator`. Slice 11d: the browser WebSocket, fed
//! by a shared subscription to the aggregator's `/fleet/subscribe`. Slice
//! 11e: `gsp-controller`'s config API, proxied the same way. Slice 11f:
//! `--static-dir` serves the built React/Vite/TS frontend (`web/`) as a
//! fallback under every route the API doesn't claim — `gsp-ui` is the one
//! process, one port an operator's browser ever talks to. Phase 12 slice 8:
//! `--users-file` (multi-operator RBAC) alongside legacy `--ui-password`,
//! and a `--hash-password` mode for populating one.

use std::io::Read;
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

    /// Password required to log in — one shared secret, implicitly `admin`.
    /// Omit to leave the UI open (no login required) — network-boundary-only
    /// auth, same posture every other optional-auth surface in this fleet
    /// has. Mutually exclusive with `--users-file`.
    #[arg(long)]
    ui_password: Option<String>,

    /// Multi-operator accounts (phase 12 slice 8): a YAML file of
    /// `{username, password_hash, role}` entries — see `gsp_ui::users`'s
    /// doc for the shape, and `--hash-password` for producing a hash.
    /// Mutually exclusive with `--ui-password`.
    #[arg(long)]
    users_file: Option<PathBuf>,

    /// A browser session ends after this many seconds without a request.
    #[arg(long, default_value_t = 1800)]
    session_idle_timeout_secs: u64,

    /// A browser session ends this many seconds after login, however
    /// active; also the session cookie's `Max-Age`.
    #[arg(long, default_value_t = 43200)]
    session_max_age_secs: u64,

    /// Most browser sessions held at once; at the cap the oldest is evicted.
    #[arg(long, default_value_t = 1000)]
    max_sessions: usize,

    /// Print an argon2 hash for a password read from stdin, then exit —
    /// does not start the server. The intended way to populate a
    /// `--users-file` entry's `password_hash`; never handle a plaintext
    /// password in a config file at rest.
    #[arg(long)]
    hash_password: bool,

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

    if args.hash_password {
        // No server, no logging setup — a one-shot CLI utility mode.
        let mut password = String::new();
        std::io::stdin().read_to_string(&mut password)?;
        println!("{}", gsp_ui::users::hash_password(password.trim_end()));
        return Ok(());
    }

    if args.ui_password.is_some() && args.users_file.is_some() {
        anyhow::bail!("--ui-password and --users-file are mutually exclusive");
    }

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

    let login_required = args.ui_password.is_some() || args.users_file.is_some();
    let mut state = AppState::new(args.ui_password)
        .with_secure_cookie(tls_cert.is_some())
        .with_session_limits(gsp_ui::session::SessionLimits {
            idle_timeout: std::time::Duration::from_secs(args.session_idle_timeout_secs),
            max_age: std::time::Duration::from_secs(args.session_max_age_secs),
            max_sessions: args.max_sessions,
        });
    if let Some(users_file) = &args.users_file {
        let users = gsp_ui::users::load(users_file)?;
        tracing::info!(
            users_file = %users_file.display(),
            user_count = users.len(),
            "loaded multi-operator accounts"
        );
        state = state.with_users(users);
    }
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
        multi_user = state.users.is_some(),
        aggregator_configured = state.aggregator.is_some(),
        controller_configured = state.controller.is_some(),
        "gsp-ui starting"
    );

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router(state))
        .fallback_service(ServeDir::new(&args.static_dir));

    gsp_http::tls::serve(args.listen, app, tls_cert, "gsp-ui").await?;

    if let Some(task) = feed_task {
        task.abort();
    }
    Ok(())
}
