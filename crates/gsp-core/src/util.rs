//! Small shared helpers.

use std::sync::LazyLock;
use std::time::Instant;

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds elapsed since process start. Monotonic; used for cheap
/// "is this due yet?" comparisons stored in atomics.
pub fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64
}
