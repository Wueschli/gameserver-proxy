//! The capability declaration a plugin embeds in its `wayhouse.plugin-caps` section.
//!
//! Only the capabilities built so far exist here; `deny_unknown_fields` makes a
//! declaration naming a capability this host does not know an install-time error rather
//! than a silently ignored (and therefore unreviewed) request.

use serde::Deserialize;

use crate::module::ModuleError;

/// The shortest timer interval the host accepts (spec: minimum 10 s).
pub const MIN_TICK_INTERVAL_SECS: u64 = 10;

/// What a plugin asks for, and what the operator approves against its sha256.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
}

/// The triggers a plugin declares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Triggers {
    #[serde(default)]
    pub on_timer: bool,
}

/// Limits of the `state` capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
        Ok(caps)
    }
}
