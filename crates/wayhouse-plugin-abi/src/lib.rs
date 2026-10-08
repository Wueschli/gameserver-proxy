//! Guest-side ABI helper for wayhouse plugins (Wave 5; design:
//! `docs/superpowers/specs/2026-10-07-plugin-automation-hooks-design.md`).
//!
//! A plugin is a WASM module the controller runs on a timer. It exports `memory`,
//! `alloc` (re-exported from this crate), `init(config_ptr, config_len)` and, when its
//! capabilities declare the timer trigger, `on_timer()`. It imports only functions from
//! the `wayhouse` namespace (see `docs/plugins.md`), each gated by a capability the
//! operator approved.
//!
//! Depending on this crate declares the ABI version: a `wayhouse.plugin-abi` custom
//! section ([`ABI_BYTES`]) is linked into every wasm32 module that uses it, and the host
//! refuses a module without it or with another version. The capabilities live in a
//! second custom section, [`CAPS_SECTION`], whose JSON the plugin author embeds.

/// ABI version this crate implements: the host accepts a module only when it declares
/// the same version (while the major is 0 a minor bump may break the ABI).
pub const ABI_MAJOR: u16 = 0;
/// See [`ABI_MAJOR`].
pub const ABI_MINOR: u16 = 1;

/// The payload of the [`ABI_SECTION`] custom section: major then minor, both u16 LE.
pub const ABI_BYTES: [u8; 4] = {
    let major = ABI_MAJOR.to_le_bytes();
    let minor = ABI_MINOR.to_le_bytes();
    [major[0], major[1], minor[0], minor[1]]
};

/// Name of the custom section that carries the ABI version.
pub const ABI_SECTION: &str = "wayhouse.plugin-abi";
/// Name of the custom section that carries the capability declaration (JSON).
pub const CAPS_SECTION: &str = "wayhouse.plugin-caps";

/// Stamps every wasm32 module that links this crate with its ABI version. Only on
/// wasm32: on a native build the attribute would put a section in the host test binary.
#[cfg(target_arch = "wasm32")]
#[used]
#[link_section = "wayhouse.plugin-abi"]
static ABI_VERSION: [u8; 4] = ABI_BYTES;

/// The host calls this to reserve `len` writable bytes before copying data in (the
/// `init` config). Delegates to the module's global allocator. Returns `0` for `len == 0`.
#[no_mangle]
pub extern "C" fn alloc(len: u32) -> u32 {
    if len == 0 {
        return 0;
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(len as usize, 1) else {
        return 0;
    };
    // SAFETY: `layout` has a non-zero size (checked above) and alignment 1.
    let ptr = unsafe { std::alloc::alloc(layout) };
    ptr as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_bytes_are_major_then_minor_le() {
        assert_eq!(ABI_BYTES, [0, 0, 1, 0]);
    }

    #[test]
    fn alloc_zero_returns_zero() {
        assert_eq!(alloc(0), 0);
    }
}
