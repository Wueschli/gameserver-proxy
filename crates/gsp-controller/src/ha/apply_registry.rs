//! Applying the registry entries (`RegisterOrigin`/`RegisterProxy`,
//! `Release`, `Touch`) of the Raft log — split out of [`super::state_machine`]
//! so that module stays about `openraft`'s bookkeeping.
//!
//! The steps are the non-HA handlers' (`crate::registry`), in their order:
//! a register claims in the address book, expands the backends against the
//! granted address (a mismatch is a `BackendHost` rejection that keeps the
//! claim) and stores the registration; a release tombstones the registry and
//! then frees the address. Everything they decide comes from the entry and
//! the replicated state — `now` from the entry, the network from
//! [`ClusterState`] — so every replica reaches the same result.
//!
//! Crash-idempotence: each database (the book, the registry) skips a step
//! whose index it already absorbed, and every entry advances both, whatever
//! the outcome. On a replay where the book step already ran, the register
//! continues from the book's `last_outcome` for this index (only the latest
//! entry can be half-applied, since entries apply one at a time).
//!
//! A storage failure is returned as `openraft`'s `StorageError` (it stops
//! the node); only deterministic refusals become `WriteResponse::Rejected`.

// Every function here returns `openraft`'s `StorageIOError`, sized by its own
// variants — not something this crate can shrink (as in `state_machine`).
#![allow(clippy::result_large_err)]

use openraft::StorageIOError;

use super::cluster_state::ClusterState;
use super::{NodeId, WriteResponse};
use crate::addresses::{expand_backends, AddressBook, Outcome, Rejection, StorageFailure};
use crate::registry::{Registration, RegistryState};
use crate::store::Applied;

/// Every function here fails only on storage, as `openraft`'s error.
type ApplyResult = Result<WriteResponse, StorageIOError<NodeId>>;

fn io<E: std::error::Error + 'static>(e: E) -> StorageIOError<NodeId> {
    StorageIOError::write_state_machine(&e)
}

/// Whether `registry` already absorbed `index`.
fn registry_done<R: Registration>(
    registry: &RegistryState<R>,
    index: u64,
) -> Result<bool, StorageIOError<NodeId>> {
    Ok(registry
        .store
        .applied_index()
        .map_err(io)?
        .is_some_and(|applied| applied >= index))
}

/// The registry step of a refused register: advance the index, store nothing.
fn reject<R: Registration>(
    registry: &RegistryState<R>,
    rejection: Rejection,
    index: u64,
) -> ApplyResult {
    registry.store.mark_applied(index).map_err(io)?;
    Ok(WriteResponse::Rejected(rejection))
}

/// `RegisterOrigin` / `RegisterProxy` at `index`.
pub(super) fn register<R: Registration>(
    registry: &RegistryState<R>,
    book: &AddressBook,
    cluster: &ClusterState,
    mut reg: R,
    now: u64,
    index: u64,
) -> ApplyResult {
    // Book step.
    let outcome = match cluster.network().map_err(io)? {
        None => book.reject_at(Rejection::NotInitialized, index),
        Some(network) => book.claim_at(
            R::ROLE,
            reg.name(),
            reg.requested_address(),
            now,
            network,
            index,
        ),
    }
    .map_err(io)?;
    let outcome = match outcome {
        Outcome::AlreadyApplied => {
            if registry_done(registry, index)? {
                return Ok(WriteResponse::Revision(None));
            }
            // The crash fell between the two steps: finish from the book's
            // record of this very claim.
            match book.last_outcome().map_err(io)? {
                Some((at, outcome)) if at == index => outcome,
                _ => {
                    return Err(io(StorageFailure(format!(
                        "the address book absorbed index {index} but holds no claim outcome for it"
                    ))))
                }
            }
        }
        decided => decided,
    };

    // Registry step.
    let assignment = match outcome {
        Outcome::Granted(a) => a,
        Outcome::Rejected(r) => return reject(registry, r, index),
        Outcome::AlreadyApplied => {
            return Err(io(StorageFailure(
                "the address book recorded `AlreadyApplied` as a claim outcome".into(),
            )))
        }
    };
    // As in the non-HA handler, the claim is kept even if the backends are
    // rejected: the owner's corrected retry gets the same address.
    if let Some(backends) = reg.backends_mut() {
        match expand_backends(backends, assignment.address) {
            Ok(expanded) => *backends = expanded,
            Err(e) => return reject(registry, Rejection::BackendHost(e), index),
        }
    }
    reg.set_tunnel_address(assignment.address);
    Ok(
        match registry.register_applied(&reg, Some(index)).map_err(io)? {
            Applied::Written(revision) => WriteResponse::Registered {
                revision,
                address: assignment.address,
            },
            Applied::AlreadyApplied => WriteResponse::Revision(None),
        },
    )
}

/// Advances both databases for an entry refused before either was
/// consulted.
fn not_initialized<R: Registration>(
    registry: &RegistryState<R>,
    book: &AddressBook,
    index: u64,
) -> ApplyResult {
    registry.store.mark_applied(index).map_err(io)?;
    book.mark_applied(index).map_err(io)?;
    Ok(WriteResponse::Rejected(Rejection::NotInitialized))
}

/// `Release` at `index`: tombstone, then free the address — `NotFound` when
/// neither the registry nor the book knows `name`.
pub(super) fn release<R: Registration>(
    registry: &RegistryState<R>,
    book: &AddressBook,
    cluster: &ClusterState,
    name: &str,
    index: u64,
) -> ApplyResult {
    if cluster.network().map_err(io)?.is_none() {
        return not_initialized(registry, book, index);
    }

    // Registry step.
    let mut response = WriteResponse::Revision(None);
    if !registry_done(registry, index)? {
        let known = registry.has_current(name).map_err(io)?
            || book.get(R::ROLE, name).map_err(io)?.is_some();
        if !known {
            registry.store.mark_applied(index).map_err(io)?;
            book.mark_applied(index).map_err(io)?;
            return Ok(WriteResponse::NotFound);
        }
        if let Applied::Written(revision) =
            registry.remove_applied(name, Some(index)).map_err(io)?
        {
            response = WriteResponse::Released {
                revision,
                address: None,
            };
        }
    }

    // Book step.
    let freed = book.release_at(R::ROLE, name, index).map_err(io)?;
    if let (WriteResponse::Released { address, .. }, Some(freed)) = (&mut response, freed) {
        *address = freed;
    }
    Ok(response)
}

/// `Touch` at `index`: `last_seen = now` in the book, no registry revision
/// (the registry's index still advances). `NotFound` for an owner the book
/// does not know.
pub(super) fn touch<R: Registration>(
    registry: &RegistryState<R>,
    book: &AddressBook,
    cluster: &ClusterState,
    name: &str,
    now: u64,
    index: u64,
) -> ApplyResult {
    if cluster.network().map_err(io)?.is_none() {
        return not_initialized(registry, book, index);
    }
    let touched = book.touch_at(R::ROLE, name, now, index).map_err(io)?;
    registry.store.mark_applied(index).map_err(io)?;
    Ok(match touched {
        Some(true) => WriteResponse::Touched,
        Some(false) => WriteResponse::NotFound,
        None => WriteResponse::Revision(None),
    })
}
