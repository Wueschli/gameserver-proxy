//! Stand-in for `sniffer_loader` in a build without the `wasm-sniffers`
//! feature (no `wasmtime`). The loader type is uninhabited, so every code
//! path that would use one is statically unreachable: the only way to get a
//! loader is [`build_sniffers`], and that always refuses. A config that sets
//! `settings.sniffers` therefore fails loudly at startup (and under
//! `--check`) instead of silently running without its plugins.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use wayhouse_config::SniffersConfig;
use wayhouse_core::sniff::{Sniffer, Sniffers};

/// Never constructed; see the module doc.
pub enum SnifferLoader {}

impl SnifferLoader {
    pub fn scan(&self, _cfg: &SniffersConfig) -> Result<HashMap<String, Arc<dyn Sniffer>>> {
        match *self {}
    }
}

pub fn build_sniffers(_cfg: &SniffersConfig) -> Result<(SnifferLoader, Sniffers)> {
    bail!(
        "this build of wayhouse was compiled without the `wasm-sniffers` cargo feature, \
         so it cannot load sniffer plugins; remove `settings.sniffers` or use a full build"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_sniffers_block_is_refused_with_a_message_naming_the_feature() {
        let cfg = SniffersConfig {
            dir: "/nonexistent".into(),
            call_timeout: Duration::from_millis(50),
            max_memory_bytes: 1 << 20,
            modules: Vec::new(),
        };
        let err = build_sniffers(&cfg).err().expect("must refuse").to_string();
        assert!(err.contains("wasm-sniffers"), "{err}");
    }
}
