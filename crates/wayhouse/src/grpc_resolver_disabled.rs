//! Stand-in for `grpc_resolver` in a build without the `grpc-resolver`
//! feature (no `tonic`/`prost`). The type is uninhabited and `new` always
//! refuses, so a config with a `kind: grpc` resolver fails at startup (and
//! under `--check`) instead of silently running without it.

use async_trait::async_trait;
use wayhouse_config::{OnError, ProxyProtocol, ResolverConfig};
use wayhouse_core::{Resolution, ResolveError, ResolveRequest, Resolver};

/// Never constructed; see the module doc.
pub enum GrpcResolver {}

impl GrpcResolver {
    pub fn new(cfg: &ResolverConfig) -> anyhow::Result<Self> {
        anyhow::bail!(
            "resolver {}: this build of wayhouse was compiled without the `grpc-resolver` cargo \
             feature, so it cannot use `kind: grpc`; use `kind: http` or a full build",
            cfg.name
        )
    }
}

#[async_trait]
impl Resolver for GrpcResolver {
    fn name(&self) -> &str {
        match *self {}
    }
    fn on_error(&self) -> OnError {
        match *self {}
    }
    fn proxy_protocol(&self) -> ProxyProtocol {
        match *self {}
    }
    fn target_connect_timeout(&self) -> std::time::Duration {
        match *self {}
    }
    fn target_idle_timeout(&self) -> std::time::Duration {
        match *self {}
    }
    async fn resolve(&self, _req: ResolveRequest) -> Result<Resolution, ResolveError> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grpc_resolver_is_refused_with_a_message_naming_the_feature() {
        let rc = ResolverConfig {
            name: "mm".into(),
            kind: wayhouse_config::ResolverKind::Grpc,
            endpoint: "http://127.0.0.1:1".into(),
            timeout: std::time::Duration::from_secs(1),
            on_error: OnError::Reject,
            cache: None,
            proxy_protocol: ProxyProtocol::None,
            target_connect_timeout: std::time::Duration::from_millis(300),
            target_idle_timeout: std::time::Duration::from_secs(90),
        };
        let err = GrpcResolver::new(&rc)
            .err()
            .expect("must refuse")
            .to_string();
        assert!(err.contains("grpc-resolver"), "{err}");
    }
}
