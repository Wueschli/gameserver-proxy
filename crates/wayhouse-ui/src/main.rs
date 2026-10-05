//! `wayhouse-ui` binary — see `lib.rs` for the design pointer and slice plan.
//! Slice 11b: session login/logout. Slice 11c: fleet reads + operational
//! verbs proxied to `wayhouse-aggregator`. Slice 11d: the browser WebSocket, fed
//! by a shared subscription to the aggregator's `/fleet/subscribe`. Slice
//! 11e: `wayhouse-controller`'s config API, proxied the same way. Slice 11f:
//! `--static-dir` serves the built React/Vite/TS frontend (`web/`) as a
//! fallback under every route the API doesn't claim — `wayhouse-ui` is the one
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

use wayhouse_ui::api::{self, AppState};

#[derive(Parser, Debug)]
#[command(
    name = "wayhouse-ui",
    version,
    about = "Operator dashboard BFF for a wayhouse fleet (docs/10 \"The admin GUI\")"
)]
struct Args {
    /// Address this UI's own HTTP server listens on.
    #[arg(long, default_value = "127.0.0.1:9903")]
    listen: SocketAddr,

    /// Password required to log in — one shared secret, implicitly `admin`.
    /// Omit to leave the UI open (no login required), which is only accepted
    /// on a loopback `--listen` (or with `--insecure-no-auth`). Mutually
    /// exclusive with `--users-file`.
    #[arg(long)]
    ui_password: Option<String>,

    /// Multi-operator accounts (phase 12 slice 8): a YAML file of
    /// `{username, password_hash, role}` entries — see `wayhouse_ui::users`'s
    /// doc for the shape, and `--hash-password` for producing a hash.
    /// Mutually exclusive with `--ui-password`.
    #[arg(long)]
    users_file: Option<PathBuf>,

    /// Bearer token `GET /metrics` requires, at least 16 bytes. The UI's own
    /// login is a browser session a scraper cannot hold, so `/metrics` has a
    /// token of its own. Omitted: `/metrics` is served only when no login is
    /// configured (an open UI); with a login it is not served at all.
    #[arg(long)]
    metrics_token: Option<String>,

    /// Allow a non-loopback `--listen` with neither `--ui-password` nor
    /// `--users-file` (an open UI). Only for deployments where the network
    /// boundary is the sole access control.
    #[arg(long)]
    insecure_no_auth: bool,

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

    /// `wayhouse-aggregator` base URL (e.g. "http://127.0.0.1:9902") that fleet
    /// reads and operational actions proxy to. Omitted: those routes return
    /// 503.
    #[arg(long)]
    aggregator_url: Option<String>,

    /// Bearer token this UI presents to `--aggregator-url`, if it requires
    /// one (its own `--auth-token`).
    #[arg(long)]
    aggregator_token: Option<String>,

    /// `wayhouse-controller` base URL (e.g. "http://127.0.0.1:9901") that
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
    /// same as running `wayhouse-ui` for its API alone (as every test in this
    /// crate does).
    #[arg(long, default_value = "crates/wayhouse-ui/web/dist")]
    static_dir: PathBuf,

    /// PEM file of extra CA certificates to trust for outbound HTTPS, in
    /// addition to the built-in Mozilla roots.
    #[arg(long)]
    ca_file: Option<PathBuf>,

    #[command(flatten)]
    tls: wayhouse_http::tls::TlsArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.hash_password {
        // No server, no logging setup — a one-shot CLI utility mode.
        let mut password = String::new();
        std::io::stdin().read_to_string(&mut password)?;
        println!("{}", wayhouse_ui::users::hash_password(password.trim_end()));
        return Ok(());
    }

    if args.ui_password.is_some() && args.users_file.is_some() {
        anyhow::bail!("--ui-password and --users-file are mutually exclusive");
    }
    wayhouse_http::policy::check_optional_secret("--metrics-token", args.metrics_token.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("WAYHOUSE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let prometheus = wayhouse_http::metrics::install("wayhouse-ui", env!("CARGO_PKG_VERSION"))?;
    wayhouse_http::policy::check_exposure(
        "wayhouse-ui",
        args.listen,
        args.ui_password.is_some() || args.users_file.is_some(),
        args.insecure_no_auth,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(path) = &args.ca_file {
        let certs = wayhouse_http::init_ca_file(path)?;
        tracing::info!(certs, path = %path.display(), "trusting extra CAs from --ca-file");
    }
    // Load (and validate) the serving certificate before anything else starts.
    let tls_cert = args.tls.load()?;

    let login_required = args.ui_password.is_some() || args.users_file.is_some();
    let mut state = AppState::new(args.ui_password)
        .with_secure_cookie(tls_cert.is_some())
        .with_session_limits(wayhouse_ui::session::SessionLimits {
            idle_timeout: std::time::Duration::from_secs(args.session_idle_timeout_secs),
            max_age: std::time::Duration::from_secs(args.session_max_age_secs),
            max_sessions: args.max_sessions,
        });
    if let Some(users_file) = &args.users_file {
        let users = wayhouse_ui::users::load(users_file)?;
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
        let feed = wayhouse_ui::fleet_feed::FleetFeed::new();
        feed_task = Some(tokio::spawn(wayhouse_ui::fleet_feed::run(
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
        "wayhouse-ui starting"
    );

    let mut app = Router::new().route("/healthz", get(|| async { "ok" }));
    if args.metrics_token.is_some() || !login_required {
        app = app.merge(wayhouse_http::metrics::router(
            prometheus,
            wayhouse_http::server::BearerAuth::new(args.metrics_token.as_deref()),
        ));
    } else {
        tracing::info!("/metrics is not served: a login is configured and no --metrics-token");
    }
    let app = app
        .merge(api::router(state))
        .fallback_service(ServeDir::new(&args.static_dir));

    wayhouse_http::tls::serve(args.listen, app, tls_cert, args.tls.limits(), "wayhouse-ui").await?;

    if let Some(task) = feed_task {
        task.abort();
    }
    Ok(())
}
