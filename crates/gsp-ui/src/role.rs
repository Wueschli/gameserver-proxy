//! Three roles (phase 12 slice 8, `docs/10` "RBAC and audit (design)"),
//! matching the verb tiers `gsp-ui`'s own API surface already has —
//! `viewer` (every read), `operator` (+ the phase-5 intent verbs fanned out
//! through the aggregator), `admin` (+ config submit/rollback/promote and
//! `POST /admin/adopt`). Not a general permission-matrix model: every route
//! this process serves already falls cleanly into one of these three
//! buckets, so a richer model would solve a problem this surface doesn't
//! have (see `docs/10`'s reasoning for the cut).
//!
//! Declared low-to-high so `#[derive(PartialOrd, Ord)]` gives "is this role
//! at least X" for free via `role >= min`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_order_low_to_high() {
        assert!(Role::Viewer < Role::Operator);
        assert!(Role::Operator < Role::Admin);
        assert!(Role::Viewer < Role::Admin);
    }

    #[test]
    fn a_role_satisfies_its_own_minimum() {
        assert!(Role::Operator >= Role::Operator);
    }
}
