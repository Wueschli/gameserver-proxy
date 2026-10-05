//! `gsp-aggregator` binary — see `lib.rs` for the design pointer and slice
//! plan. `POST /ingest`, `GET /fleet/*`, intent-verb fan-out
//! (`crate::fanout`), and `--auth-token`/`--instance-token` (slice 10) are
//! all built; `GET /healthz` stays unauthenticated, wired up separately from
//! `api::router`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use gsp_aggregator::api::{self, AppState};
use gsp_aggregator::ingest::IngestStore;
use gsp_aggregator::parent_push;

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

    /// Bearer token every request except /healthz must present, at least 16
    /// bytes. Omit to leave this aggregator's own API open, which is only
    /// accepted on a loopback `--listen` (or with `--insecure-no-auth`).
    #[arg(long)]
    auth_token: Option<String>,

    /// Bearer token `POST /ingest` accepts instead of `--auth-token`, at least
    /// 16 bytes, so the proxies that push telemetry do not hold a credential
    /// that also unlocks `/fleet/*` (including drain and backend edits). It
    /// unlocks only `/ingest`; `--auth-token` then gates `/fleet/*` alone.
    /// Must differ from `--auth-token`. Omitted: `/ingest` is gated by
    /// `--auth-token` like the rest of the API.
    #[arg(long)]
    ingest_token: Option<String>,

    /// Which `admin_url` hosts an ingested push may report (the fan-out calls
    /// them with `--instance-token`): an IP/CIDR (`10.0.0.0/8`), a hostname, or
    /// `*.suffix`; repeat or comma-separate. Omitted: the URL's host must be
    /// an IP literal equal to the pushing connection's source address.
    #[arg(long, value_delimiter = ',')]
    instance_url_allow: Vec<String>,

    /// Bearer token `GET /metrics` accepts instead of `--auth-token`, at least
    /// 16 bytes, so a Prometheus scraper need not hold the admin token. It
    /// unlocks only `/metrics`. Omitted: `/metrics` is gated by `--auth-token`
    /// like the rest of the API.
    #[arg(long)]
    metrics_token: Option<String>,

    /// Allow a non-loopback `--listen` with no `--auth-token`. Only for
    /// deployments where the network boundary is the sole access control.
    #[arg(long)]
    insecure_no_auth: bool,

    /// Bearer token this aggregator presents when fanning intent verbs out
    /// to each instance's admin API (`settings.admin.auth_token` on `gsp`).
    /// A separate secret from `--auth-token`: one gates calls into this
    /// aggregator, this one is what it presents going out.
    #[arg(long)]
    instance_token: Option<String>,

    /// Parent aggregator's base URL — when set, this tier also pushes its
    /// own merged view up to it (phase 12 slice 2, see `docs/10` "Fleet
    /// topology"). Requires `--tier-name`.
    #[arg(long)]
    parent_url: Option<String>,

    /// This tier's name, used to namespace instances pushed to
    /// `--parent-url` as `"{tier_name}/{instance}"` so two tiers' instances
    /// never collide in the parent's flat store. Required with
    /// `--parent-url`.
    #[arg(long)]
    tier_name: Option<String>,

    /// Bearer token this tier presents pushing to `--parent-url`.
    #[arg(long)]
    parent_token: Option<String>,

    /// How often this tier pushes its merged view to `--parent-url`.
    #[arg(long, default_value_t = 10)]
    parent_push_interval_sec: u64,

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

    if args.parent_url.is_some() && args.tier_name.is_none() {
        anyhow::bail!("--parent-url requires --tier-name");
    }
    gsp_http::policy::check_optional_secret("--auth-token", args.auth_token.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;
    gsp_http::policy::check_optional_secret("--ingest-token", args.ingest_token.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;
    if args.ingest_token.is_some() && args.ingest_token == args.auth_token {
        anyhow::bail!("--ingest-token must differ from --auth-token, or it separates nothing");
    }
    let admin_url_policy = gsp_aggregator::trust::AdminUrlPolicy::new(&args.instance_url_allow)
        .map_err(|e| anyhow::anyhow!("--instance-url-allow: {e}"))?;
    gsp_http::policy::check_optional_secret("--metrics-token", args.metrics_token.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let prometheus = gsp_http::metrics::install("gsp-aggregator", env!("CARGO_PKG_VERSION"))?;
    gsp_http::policy::check_exposure(
        "gsp-aggregator",
        args.listen,
        args.auth_token.is_some(),
        args.insecure_no_auth,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(path) = &args.ca_file {
        let certs = gsp_http::init_ca_file(path)?;
        tracing::info!(certs, path = %path.display(), "trusting extra CAs from --ca-file");
    }
    // Load (and validate) the serving certificate before anything else starts.
    let tls_cert = args.tls.load()?;

    // No data-dir, no persistence — the store is deliberately in-memory
    // only (see the "stateless and ephemeral by design" note in lib.rs).
    let store = Arc::new(IngestStore::new());
    let state = AppState::new(store)
        .with_auth_token(args.auth_token.clone())
        .with_ingest_token(args.ingest_token.clone())
        .with_admin_url_policy(admin_url_policy)
        .with_instance_token(args.instance_token);

    tracing::info!(
        auth = args.auth_token.is_some(),
        parent = args.parent_url.is_some(),
        "aggregator store initialized"
    );

    if let Some(parent_url) = args.parent_url {
        let cfg = parent_push::PushConfig {
            base_url: parent_url,
            tier_name: args.tier_name.expect("checked above"),
            interval: Duration::from_secs(args.parent_push_interval_sec),
            token: args.parent_token,
        };
        tokio::spawn(parent_push::run(cfg, state.store.clone()));
    }

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(gsp_http::metrics::router(
            prometheus,
            gsp_http::server::BearerAuth::new(
                args.metrics_token.as_deref().or(args.auth_token.as_deref()),
            ),
        ))
        .merge(api::router(state));

    gsp_http::tls::serve(
        args.listen,
        app,
        tls_cert,
        args.tls.limits(),
        "gsp-aggregator",
    )
    .await?;

    Ok(())
}
