//! Small shared helpers.

use std::sync::LazyLock;
use std::time::Instant;

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds elapsed since process start. Monotonic; used for cheap
/// "is this due yet?" comparisons stored in atomics.
pub fn mono_ms() -> u64 {
    START.elapsed().as_millis() as u64
}

/// Whether a socket error is this process running out of a local resource
/// (file descriptors, memory, buffers, ephemeral ports) rather than the
/// backend misbehaving. Health logic treats these as "unknown" so that a
/// proxy-side shortage never ejects backends.
pub fn is_local_resource_error(e: &std::io::Error) -> bool {
    use nix::errno::Errno;
    matches!(
        e.raw_os_error().map(Errno::from_raw),
        Some(Errno::EMFILE | Errno::ENFILE | Errno::ENOMEM | Errno::ENOBUFS | Errno::EADDRNOTAVAIL)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_resource_errors_are_told_apart_from_backend_errors() {
        for code in [24, 23, 12, 105, 99] {
            assert!(is_local_resource_error(&std::io::Error::from_raw_os_error(
                code
            )));
        }
        assert!(!is_local_resource_error(
            &std::io::Error::from_raw_os_error(111)
        )); // ECONNREFUSED
        assert!(!is_local_resource_error(&std::io::Error::other("x")));
    }
}
