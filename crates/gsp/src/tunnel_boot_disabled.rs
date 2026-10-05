//! Stand-in for `tunnel_boot` in a build without the `tunnel` feature (no
//! `defguard_wireguard_rs`, netlink). `config` refuses `--tunnel-iface`, so the
//! flags fail at startup (and under `--check`) instead of silently running a
//! proxy that never forwards tunneled traffic. The types are uninhabited.

use crate::tunnel_source::TunnelRegistry;
use crate::Args;

/// Never constructed; see the module doc.
pub enum TunnelConfig {}

/// Never constructed; see the module doc.
pub enum Running {}

pub fn config(args: &Args) -> anyhow::Result<Option<TunnelConfig>> {
    if args.tunnel_iface.is_some() {
        anyhow::bail!(
            "--tunnel-iface: this build of gsp was compiled without the `tunnel` cargo \
             feature, so it cannot bring up the WireGuard backend transport; remove the \
             `--tunnel-*` flags or use a full build"
        );
    }
    Ok(None)
}

pub fn registry(tc: &TunnelConfig) -> TunnelRegistry {
    match *tc {}
}

#[allow(clippy::unused_async)] // same signature as the real `start`
pub async fn start(tc: TunnelConfig) -> anyhow::Result<Running> {
    match tc {}
}

impl Running {
    pub fn stop(self) {
        match self {}
    }
}
