//! The capability declaration a plugin embeds in its `wayhouse.plugin-caps` section.
//!
//! Only the capabilities built so far exist here; `deny_unknown_fields` makes a
//! declaration naming a capability this host does not know an install-time error rather
//! than a silently ignored (and therefore unreviewed) request.

use serde::{Deserialize, Serialize};

use crate::module::ModuleError;

/// The shortest timer interval the host accepts (spec: minimum 10 s).
pub const MIN_TICK_INTERVAL_SECS: u64 = 10;

/// Host ceiling on a declared `state.max_bytes` (1 MiB).
pub const MAX_STATE_BYTES: usize = 1024 * 1024;

/// What a plugin asks for, and what the operator approves against its sha256.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Triggers the host may deliver. An undeclared trigger is never delivered.
    #[serde(default)]
    pub triggers: Triggers,
    /// Seconds between `on_timer` calls; required (at least [`MIN_TICK_INTERVAL_SECS`])
    /// when `triggers.on_timer` is set.
    #[serde(default)]
    pub tick_interval_secs: u64,
    /// May call the `log` import.
    #[serde(default)]
    pub log: bool,
    /// May call `state_get` / `state_put`, within this cap.
    #[serde(default)]
    pub state: Option<StateCap>,
    /// May call `http_request` / `http_read` to these HTTPS hosts only.
    #[serde(default)]
    pub http: Option<HttpCap>,
    /// Secret slots the plugin reads through `${secret:NAME}` in request headers; each is
    /// bound to hosts of the `http` list and approved with them.
    #[serde(default)]
    pub secrets: Vec<SecretSlot>,
}

/// Most hosts an `http` declaration may list.
pub const MAX_HTTP_HOSTS: usize = 16;
/// Most secret slots a plugin may declare.
pub const MAX_SECRET_SLOTS: usize = 8;

/// The `http` capability: which HTTPS endpoints the plugin may call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpCap {
    pub hosts: Vec<HttpHost>,
}

/// One approved HTTPS endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpHost {
    /// A lowercase DNS name or an IP literal; no wildcards, scheme, port or path.
    pub host: String,
    #[serde(default = "https_port")]
    pub port: u16,
    /// Whether the host may resolve to a private, loopback or link-local address. Off by
    /// default; the approval screen shows it as such.
    #[serde(default)]
    pub allow_private: bool,
}

fn https_port() -> u16 {
    443
}

impl HttpHost {
    pub fn matches(&self, host: &str, port: u16) -> bool {
        self.host == host && self.port == port
    }
}

/// A secret slot a plugin may use, bound to the hosts that may receive it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSlot {
    /// `A-Z`, `0-9` and `_`, starting with a letter.
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Hosts (names from the `http` list) a request carrying this secret may go to.
    pub hosts: Vec<String>,
}

/// Whether `name` is a valid secret slot name.
pub fn valid_slot_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['-', '.'])
        && !host.ends_with(['-', '.'])
        && !host.contains("..")
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// The triggers a plugin declares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Triggers {
    #[serde(default)]
    pub on_timer: bool,
}

/// Limits of the `state` capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StateCap {
    /// Cap on the sum of key and value bytes the plugin's state may hold.
    pub max_bytes: usize,
}

impl Capabilities {
    /// Whether everything this declaration asks for is within `approved`. A module that
    /// asks for more than the operator approved (a new trigger, `log`, `state`, or a
    /// bigger state cap) needs a new approval before it can load.
    pub fn check_within(&self, approved: &Capabilities) -> Result<(), ModuleError> {
        let over = |what: &str| Err(ModuleError::CapsNotApproved(what.to_string()));
        if self.triggers.on_timer && !approved.triggers.on_timer {
            return over("trigger on_timer");
        }
        if self.log && !approved.log {
            return over("log");
        }
        if let Some(want) = &self.http {
            let Some(ok) = &approved.http else {
                return over("http");
            };
            for h in &want.hosts {
                let granted = ok
                    .hosts
                    .iter()
                    .any(|a| a.matches(&h.host, h.port) && (a.allow_private || !h.allow_private));
                if !granted {
                    return over(&format!("http access to {}:{}", h.host, h.port));
                }
            }
        }
        for slot in &self.secrets {
            let granted = approved
                .secrets
                .iter()
                .find(|a| a.name == slot.name)
                .is_some_and(|a| slot.hosts.iter().all(|h| a.hosts.contains(h)));
            if !granted {
                return over(&format!("secret slot {}", slot.name));
            }
        }
        match (self.state, approved.state) {
            (None, _) => {}
            (Some(_), None) => return over("state"),
            (Some(want), Some(ok)) if want.max_bytes > ok.max_bytes => {
                return over("a larger state cap");
            }
            (Some(_), Some(_)) => {}
        }
        Ok(())
    }

    /// Parse and validate the section payload.
    pub fn parse(bytes: &[u8]) -> Result<Self, ModuleError> {
        let caps: Self =
            serde_json::from_slice(bytes).map_err(|e| ModuleError::CapsInvalid(e.to_string()))?;
        if caps.triggers.on_timer {
            if caps.tick_interval_secs == 0 {
                return Err(ModuleError::UndeclaredTickInterval);
            }
            if caps.tick_interval_secs < MIN_TICK_INTERVAL_SECS {
                return Err(ModuleError::TickIntervalTooShort(caps.tick_interval_secs));
            }
        }
        if let Some(state) = caps.state {
            if state.max_bytes > MAX_STATE_BYTES {
                return Err(ModuleError::CapsInvalid(format!(
                    "state.max_bytes is {}, above the host ceiling of {MAX_STATE_BYTES}",
                    state.max_bytes
                )));
            }
        }
        caps.validate_network()?;
        Ok(caps)
    }

    fn validate_network(&self) -> Result<(), ModuleError> {
        let bad = |why: String| Err(ModuleError::CapsInvalid(why));
        if let Some(http) = &self.http {
            if http.hosts.is_empty() || http.hosts.len() > MAX_HTTP_HOSTS {
                return bad(format!("http.hosts needs 1 to {MAX_HTTP_HOSTS} entries"));
            }
            for h in &http.hosts {
                if !valid_host(&h.host) || h.port == 0 {
                    return bad(format!(
                        "http host {:?}: use a lowercase DNS name or IP address and a port",
                        h.host
                    ));
                }
            }
        }
        if self.secrets.len() > MAX_SECRET_SLOTS {
            return bad(format!("more than {MAX_SECRET_SLOTS} secret slots"));
        }
        for (i, slot) in self.secrets.iter().enumerate() {
            if !valid_slot_name(&slot.name) {
                return bad(format!(
                    "secret slot {:?}: use A-Z, 0-9 and _, starting with a letter",
                    slot.name
                ));
            }
            if self.secrets[..i].iter().any(|s| s.name == slot.name) {
                return bad(format!("secret slot {} is declared twice", slot.name));
            }
            if slot.description.chars().count() > 200 {
                return bad(format!(
                    "secret slot {}: description over 200 characters",
                    slot.name
                ));
            }
            let Some(http) = &self.http else {
                return bad(format!(
                    "secret slot {} needs the http capability",
                    slot.name
                ));
            };
            if slot.hosts.is_empty()
                || !slot
                    .hosts
                    .iter()
                    .all(|h| http.hosts.iter().any(|a| &a.host == h))
            {
                return bad(format!(
                    "secret slot {}: hosts must be a non-empty subset of the http hosts",
                    slot.name
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTTP: &str = r#"{"http":{"hosts":[{"host":"panel.example","port":8443},{"host":"10.0.0.5","allow_private":true}]},
        "secrets":[{"name":"PANEL_TOKEN","hosts":["panel.example"]}]}"#;

    fn parse(json: &str) -> Result<Capabilities, ModuleError> {
        Capabilities::parse(json.as_bytes())
    }

    #[test]
    fn http_hosts_and_secret_slots_parse_with_their_defaults() {
        let c = parse(HTTP).unwrap();
        let hosts = &c.http.as_ref().unwrap().hosts;
        assert_eq!((hosts[0].port, hosts[0].allow_private), (8443, false));
        assert_eq!((hosts[1].port, hosts[1].allow_private), (443, true));
        assert_eq!(c.secrets[0].name, "PANEL_TOKEN");
    }

    #[test]
    fn malformed_network_declarations_are_refused() {
        for bad in [
            r#"{"http":{"hosts":[]}}"#,
            r#"{"http":{"hosts":[{"host":"Panel.Example"}]}}"#,
            r#"{"http":{"hosts":[{"host":"https://panel.example"}]}}"#,
            r#"{"http":{"hosts":[{"host":"*.example"}]}}"#,
            r#"{"http":{"hosts":[{"host":"a.example","port":0}]}}"#,
            r#"{"secrets":[{"name":"T","hosts":["a.example"]}]}"#,
            r#"{"http":{"hosts":[{"host":"a.example"}]},"secrets":[{"name":"lower","hosts":["a.example"]}]}"#,
            r#"{"http":{"hosts":[{"host":"a.example"}]},"secrets":[{"name":"T","hosts":["b.example"]}]}"#,
            r#"{"http":{"hosts":[{"host":"a.example"}]},"secrets":[{"name":"T","hosts":[]}]}"#,
            r#"{"http":{"hosts":[{"host":"a.example"}]},"secrets":[{"name":"T","hosts":["a.example"]},{"name":"T","hosts":["a.example"]}]}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_module_cannot_ask_for_more_network_than_was_approved() {
        let approved = parse(HTTP).unwrap();
        assert!(approved.check_within(&approved).is_ok());
        let no_http = parse(r#"{"log":true}"#).unwrap();
        let want = |json: &str| parse(json).unwrap();
        assert!(want(HTTP).check_within(&no_http).is_err());
        // A host that was not approved, the same host on another port, and private access
        // where only public was approved.
        for json in [
            r#"{"http":{"hosts":[{"host":"other.example"}]}}"#,
            r#"{"http":{"hosts":[{"host":"panel.example","port":443}]}}"#,
            r#"{"http":{"hosts":[{"host":"panel.example","port":8443,"allow_private":true}]}}"#,
        ] {
            assert!(want(json).check_within(&approved).is_err(), "{json}");
        }
        // A new secret slot, and a slot bound to more hosts than approved.
        let narrow = parse(
            r#"{"http":{"hosts":[{"host":"panel.example","port":8443},{"host":"10.0.0.5","allow_private":true}]},
                "secrets":[{"name":"PANEL_TOKEN","hosts":["panel.example"]}]}"#,
        )
        .unwrap();
        for json in [
            r#"{"http":{"hosts":[{"host":"panel.example","port":8443}]},"secrets":[{"name":"OTHER","hosts":["panel.example"]}]}"#,
            r#"{"http":{"hosts":[{"host":"panel.example","port":8443},{"host":"10.0.0.5","allow_private":true}]},
                "secrets":[{"name":"PANEL_TOKEN","hosts":["panel.example","10.0.0.5"]}]}"#,
        ] {
            assert!(want(json).check_within(&narrow).is_err(), "{json}");
        }
        assert!(
            want(r#"{"http":{"hosts":[{"host":"panel.example","port":8443}]}}"#)
                .check_within(&narrow)
                .is_ok()
        );
    }

    #[test]
    fn slot_names_are_upper_snake() {
        assert!(valid_slot_name("PANEL_TOKEN_2"));
        for bad in ["", "panel", "2FA", "A-B", "A B"] {
            assert!(!valid_slot_name(bad), "{bad}");
        }
    }
}
