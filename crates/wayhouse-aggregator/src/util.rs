//! Tiny helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch — wall clock. Used only for "how long
/// ago did we last hear from this instance" staleness math (`ingest`), never
/// for anything ordering-sensitive.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
