//! `standalone` vs. `slave` — an install-time fact about this controller
//! tier, install-time no longer meaning "fixed for the process lifetime"
//! since phase 12 slice 5 added [`crate::adopt`] (`docs/10` "Controller
//! role", "Adoption").
//!
//! `standalone` is today's phase 10+11 behaviour unchanged: the tier accepts
//! writes directly and is the top of its own subtree. `slave` never accepts
//! a write of its own — every write must originate at the root and arrive
//! here via [`crate::parent_client`] / [`crate::intent::relay`], the only
//! callers allowed to bypass the gate (they write straight to
//! [`crate::store::Store::put`] via `apply_revision`). The role is never
//! *inferred* from connectivity: a `slave` that loses its parent freezes on
//! last-known-good, it does not promote itself back to `standalone` on its
//! own (`docs/10` "never inferred from connectivity") — the only way this
//! process's role ever changes post-startup is the explicit, operator-
//! triggered flip in [`crate::adopt`].
//!
//! [`RoleHandle`] is the shared, mutable cell both `crate::api::AppState`
//! and `crate::intent::api::IntentState` hold a clone of — a plain `Role`
//! field would go stale on one of the two states the moment `adopt` flips
//! the other. A `RwLock` (not an atomic) because `Role` isn't representable
//! as a single machine word without inventing an encoding for one; this is
//! a control-plane fact read/written a handful of times per request, not a
//! hot path.

use std::fmt;
use std::sync::{Arc, RwLock};

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

/// A shared, cloneable handle onto this tier's current [`Role`]. Every clone
/// reads/writes the same underlying cell.
#[derive(Clone)]
pub struct RoleHandle(Arc<RwLock<Role>>);

impl RoleHandle {
    pub fn new(role: Role) -> Self {
        RoleHandle(Arc::new(RwLock::new(role)))
    }

    pub fn get(&self) -> Role {
        *self.0.read().expect("role lock poisoned")
    }

    pub fn set(&self, role: Role) {
        *self.0.write().expect("role lock poisoned") = role;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_observes_a_set_made_through_the_original() {
        let handle = RoleHandle::new(Role::Standalone);
        let clone = handle.clone();
        assert_eq!(clone.get(), Role::Standalone);
        handle.set(Role::Slave);
        assert_eq!(clone.get(), Role::Slave);
    }
}
