//! Plugin routes merged into a config document (`docs/plugins.md` "Routes", #236).
//!
//! The controller publishes the routes enabled plugins declare; a proxy applies them with
//! [`merge_text`] to the operator's document before parsing it. Only listeners that opted
//! in (`plugin_routes:`) get them, and they are appended after the listener's own routes,
//! so an operator route, including an `always` fallback, always wins. Each distinct backend
//! becomes a one-target pool named `plugin-<hash of the address>`.

use serde_norway::{Mapping, Value};
use sha2::{Digest, Sha256};

use crate::ConfigError;

/// One route a plugin declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRoute {
    /// A hostname or `*.suffix` pattern.
    pub host: String,
    /// `ip:port`.
    pub backend: String,
}

/// What a merge did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Merged {
    pub text: String,
    /// Routes left out, with the reason (a pool name an operator already uses).
    pub skipped: Vec<String>,
    /// Plugin routes added to listeners, counted per listener.
    pub applied: usize,
}

/// The pool a plugin backend gets.
pub fn pool_name(backend: &str) -> String {
    let digest = Sha256::digest(backend.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("plugin-{hex}")
}

fn key(s: &str) -> Value {
    Value::String(s.to_string())
}

fn listeners_with_opt_in(doc: &Value) -> bool {
    doc.get("listeners")
        .and_then(Value::as_sequence)
        .is_some_and(|ls| ls.iter().any(|l| l.get("plugin_routes").is_some()))
}

/// Whether any listener of `text` opted in to plugin routes. A document that does not parse
/// as YAML counts as not wanting them (the caller's own parse reports the error).
pub fn wants_plugin_routes(text: &str) -> bool {
    serde_norway::from_str::<Value>(text).is_ok_and(|d| listeners_with_opt_in(&d))
}

/// `text` with `routes` appended to every opted-in listener. With no routes, or no opted-in
/// listener, the text comes back byte for byte.
pub fn merge_text(text: &str, routes: &[PluginRoute]) -> Result<Merged, ConfigError> {
    let unchanged = || Merged {
        text: text.to_string(),
        ..Merged::default()
    };
    if routes.is_empty() {
        return Ok(unchanged());
    }
    let mut doc: Value = serde_norway::from_str(text)?;
    if !listeners_with_opt_in(&doc) {
        return Ok(unchanged());
    }
    let mut out = Merged::default();
    let operator_pools: Vec<String> = doc
        .get("pools")
        .and_then(Value::as_sequence)
        .map(|ps| {
            ps.iter()
                .filter_map(|p| p.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // One pool per distinct backend; a name an operator pool already has is not reused.
    let mut new_pools: Vec<(String, String)> = Vec::new();
    let mut usable: Vec<(&PluginRoute, String)> = Vec::new();
    for r in routes {
        let name = pool_name(&r.backend);
        if operator_pools.contains(&name) {
            out.skipped.push(format!(
                "{} -> {}: pool name {name} is taken",
                r.host, r.backend
            ));
            continue;
        }
        if !new_pools.iter().any(|(n, _)| *n == name) {
            new_pools.push((name.clone(), r.backend.clone()));
        }
        usable.push((r, name));
    }

    let Some(listeners) = doc.get_mut("listeners").and_then(Value::as_sequence_mut) else {
        return Ok(unchanged());
    };
    for l in listeners {
        let Some(mode) = l.get("plugin_routes").cloned() else {
            continue;
        };
        let Some(map) = l.as_mapping_mut() else {
            continue;
        };
        // A bare `pool:` is shorthand for one `always` route: spell it out so the plugin
        // routes can follow it.
        if let Some(pool) = map.remove("pool") {
            let mut m = Mapping::new();
            m.insert(key("type"), key("always"));
            let mut a = Mapping::new();
            a.insert(key("pool"), pool);
            let mut route = Mapping::new();
            route.insert(key("match"), Value::Mapping(m));
            route.insert(key("action"), Value::Mapping(a));
            let routes = map
                .entry(key("routes"))
                .or_insert_with(|| Value::Sequence(vec![]));
            if let Some(seq) = routes.as_sequence_mut() {
                seq.insert(0, Value::Mapping(route));
            }
        }
        let kind = mode.get("type").cloned().unwrap_or_else(|| key("sni"));
        let sniffer = mode.get("sniffer").cloned();
        let seq = map
            .entry(key("routes"))
            .or_insert_with(|| Value::Sequence(vec![]));
        let Some(seq) = seq.as_sequence_mut() else {
            continue;
        };
        for (r, pool) in &usable {
            let mut m = Mapping::new();
            m.insert(key("type"), kind.clone());
            if let Some(s) = &sniffer {
                m.insert(key("sniffer"), s.clone());
            }
            m.insert(key("host"), Value::Sequence(vec![key(&r.host)]));
            let mut a = Mapping::new();
            a.insert(key("pool"), key(pool));
            let mut route = Mapping::new();
            route.insert(key("match"), Value::Mapping(m));
            route.insert(key("action"), Value::Mapping(a));
            seq.push(Value::Mapping(route));
            out.applied += 1;
        }
    }

    if out.applied > 0 {
        let pools = doc
            .as_mapping_mut()
            .expect("a document with listeners is a mapping")
            .entry(key("pools"))
            .or_insert_with(|| Value::Sequence(vec![]));
        if let Some(seq) = pools.as_sequence_mut() {
            for (name, backend) in new_pools {
                let mut p = Mapping::new();
                p.insert(key("name"), key(&name));
                p.insert(key("targets"), Value::Sequence(vec![key(&backend)]));
                seq.push(Value::Mapping(p));
            }
        }
    }
    out.text = serde_norway::to_string(&doc)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_str, Action, Matcher};

    const BASE: &str = r#"
schema_version: 2
pools:
  - name: lobby
    targets: ["10.9.0.1:25565"]
listeners:
  - name: mc
    bind: "0.0.0.0:25565"
    protocol: tcp
    plugin_routes: { type: sni }
    routes:
      - match: { type: sni, host: ["lobby.example.com"] }
        action: { pool: lobby }
"#;

    fn r(host: &str, backend: &str) -> PluginRoute {
        PluginRoute {
            host: host.into(),
            backend: backend.into(),
        }
    }

    #[test]
    fn plugin_routes_follow_the_operators_and_get_one_pool_per_backend() {
        let m = merge_text(
            BASE,
            &[
                r("a.example.com", "10.0.1.1:25565"),
                r("b.example.com", "10.0.1.1:25565"),
                r("*.eu.example.com", "10.0.1.2:25565"),
            ],
        )
        .unwrap();
        assert_eq!(m.applied, 3);
        let cfg = parse_str(&m.text).unwrap();
        assert_eq!(cfg.pools.len(), 3, "lobby plus two plugin pools");
        let l = &cfg.listeners[0];
        assert_eq!(l.routes.len(), 4);
        let Action::Pool(first) = &l.routes[0].action else {
            panic!()
        };
        assert_eq!(first, "lobby", "the operator's route stays first");
        let Action::Pool(second) = &l.routes[1].action else {
            panic!()
        };
        assert_eq!(*second, pool_name("10.0.1.1:25565"));
        assert!(matches!(l.routes[1].matcher, Matcher::Sni { .. }));
    }

    #[test]
    fn a_bare_pool_listener_keeps_its_pool_ahead_of_the_plugin_routes() {
        let text = r#"
schema_version: 2
pools:
  - name: lobby
    targets: ["10.9.0.1:25565"]
listeners:
  - name: mc
    bind: "0.0.0.0:25565"
    protocol: tcp
    pool: lobby
    plugin_routes: { type: sni }
"#;
        let m = merge_text(text, &[r("a.example.com", "10.0.1.1:25565")]).unwrap();
        let cfg = parse_str(&m.text).unwrap();
        let routes = &cfg.listeners[0].routes;
        assert_eq!(routes.len(), 2);
        assert!(matches!(routes[0].matcher, Matcher::Always));
    }

    #[test]
    fn nothing_changes_without_routes_or_an_opted_in_listener() {
        assert_eq!(merge_text(BASE, &[]).unwrap().text, BASE);
        let plain = BASE.replace("    plugin_routes: { type: sni }\n", "");
        let m = merge_text(&plain, &[r("a.example.com", "10.0.1.1:1")]).unwrap();
        assert_eq!((m.text.as_str(), m.applied), (plain.as_str(), 0));
        assert!(wants_plugin_routes(BASE));
        assert!(!wants_plugin_routes(&plain));
    }

    #[test]
    fn a_pool_name_an_operator_already_uses_is_not_reused() {
        let taken = pool_name("10.0.1.1:25565");
        let text = BASE
            .replace("name: lobby", &format!("name: {taken}"))
            .replace("pool: lobby", &format!("pool: {taken}"));
        let m = merge_text(&text, &[r("a.example.com", "10.0.1.1:25565")]).unwrap();
        assert_eq!(m.applied, 0);
        assert_eq!(m.skipped.len(), 1);
    }

    #[test]
    fn the_opt_in_field_is_checked() {
        for bad in [
            "plugin_routes: { type: nope }",
            "plugin_routes: { type: sniffer }",
            "plugin_routes: { type: sni, sniffer: minecraft }",
        ] {
            let text = BASE.replace("plugin_routes: { type: sni }", bad);
            assert!(parse_str(&text).is_err(), "{bad}");
        }
        let udp = "schema_version: 2\npools:\n  - name: p\n    targets: [\"10.9.0.1:1\"]\nlisteners:\n  - name: u\n    bind: \"0.0.0.0:1\"\n    protocol: udp\n    pool: p\n    plugin_routes: { type: sni }\n";
        assert!(parse_str(udp)
            .unwrap_err()
            .to_string()
            .contains("tcp listeners"));
        // The field needs a schema that has it.
        let old = BASE.replace("schema_version: 2\n", "schema_version: 1\n");
        assert!(parse_str(&old)
            .unwrap_err()
            .to_string()
            .contains("schema_version 2"));
    }
}
