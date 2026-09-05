//! `standalone` vs. `slave` — a static, install-time fact about this
//! controller tier (`docs/10` "Controller role", phase 12 slice 1).
//!
//! `standalone` is today's phase 10+11 behaviour unchanged: the tier accepts
//! writes directly and is the top of its own subtree. `slave` never accepts
//! a write of its own — every write must originate at the root and arrive
//! here via [`crate::parent_client`], which is the *only* caller allowed to
//! bypass [`Role::require_standalone`] (it writes straight to
//! [`crate::store::Store::put`]). The role is never inferred from
//! connectivity: a `slave` that loses its parent freezes on last-known-good,
//! it does not promote itself (see `docs/10` "never inferred from
//! connectivity").

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    Standalone,
    Slave,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::Standalone => write!(f, "standalone"),
            Role::Slave => write!(f, "slave"),
        }
    }
}
