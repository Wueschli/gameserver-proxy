//! Guest-side ABI helper for wayhouse sniffers (phase 9 slice 5).
//!
//! Implements the *write* side of the ABI the host loader documents
//! (`crates/wayhouse/src/sniffer_loader.rs`, module doc): a bump-free allocator —
//! it just delegates to the module's own global allocator — behind
//! `alloc(len) -> ptr`, and an encoder for the compact `RouteHint` result
//! format the host reads back:
//!
//! ```text
//! byte 0:            flags (bit0 = reject, bit1 = host present, bit2 = key present)
//! if host present:    u16 LE length, then that many UTF-8 bytes
//! if key present:     u16 LE length, then that many UTF-8 bytes
//! ```
//!
//! Depending on this crate is also what declares the sniffer's ABI version: a
//! `wayhouse.abi` custom section ([`ABI_BYTES`]) is linked into every module
//! that uses it, and the host refuses a module without it or with another version.
//!
//! A sniffer crate only needs three things: depend on this crate, implement
//! `#[no_mangle] pub extern "C" fn sniff(in_ptr: u32, in_len: u32, cfg_ptr: u32,
//! cfg_len: u32) -> i64` calling [`input`] to borrow the peeked bytes the host
//! wrote, [`config`] to borrow this module's `settings.sniffers.modules[].config`
//! string (empty when unset), and [`emit_hint`] / [`NOT_RECOGNISED`] to return,
//! and declare `crate-type = ["cdylib"]`. This crate re-exports [`alloc`] as the
//! module's `alloc` export — a sniffer does not implement its own.
//!
//! The host writes the config region with the same `alloc` + copy it uses for
//! the input, immediately before each `sniff` call; `cfg_len` is `0` (and
//! `cfg_ptr` meaningless) when the module has no configured `config`.
//!
//! No I/O, no host imports: everything here is pure byte munging over the
//! module's own linear memory, matching the sandbox guarantee that a sniffer
//! cannot reach the filesystem, clock, or network (there's nothing here that
//! could).

/// ABI version this crate implements: the host accepts a module only when it
/// declares the same version (while the major is 0 a minor bump may break the
/// ABI). Keep in step with `HOST_ABI` in `crates/wayhouse/src/sniffer_loader.rs`;
/// the `built_sniffers_declare_the_host_abi` test there fails if they drift.
pub const ABI_MAJOR: u16 = 0;
/// See [`ABI_MAJOR`].
pub const ABI_MINOR: u16 = 1;

/// The payload of the `wayhouse.abi` custom section: major then minor, both u16 LE.
pub const ABI_BYTES: [u8; 4] = {
    let major = ABI_MAJOR.to_le_bytes();
    let minor = ABI_MINOR.to_le_bytes();
    [major[0], major[1], minor[0], minor[1]]
};

/// Stamps every module that links this crate with its ABI version, so the host
/// can read it without instantiating the module. Only on wasm32: on a native
/// build the attribute would put a section in the host test binary.
#[cfg(target_arch = "wasm32")]
#[used]
#[link_section = "wayhouse.abi"]
static ABI_VERSION: [u8; 4] = ABI_BYTES;

/// The host calls this once per `sniff()` call to reserve `len` writable
/// bytes for the input it's about to copy in. Delegates to the module's
/// normal global allocator (`dlmalloc` on `wasm32-unknown-unknown`) rather
/// than a hand-rolled bump pointer, so it never collides with allocations a
/// sniffer makes itself (e.g. inside [`emit_hint`]).
///
/// Returns `0` if `len` is `0` (nothing to allocate — matches
/// `std::alloc::alloc`'s documented "do not call with a zero-size layout").
#[no_mangle]
pub extern "C" fn alloc(len: u32) -> u32 {
    if len == 0 {
        return 0;
    }
    let layout = match std::alloc::Layout::from_size_align(len as usize, 1) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    // SAFETY: `layout` has a non-zero size (checked above) and alignment 1,
    // which is always valid.
    let ptr = unsafe { std::alloc::alloc(layout) };
    ptr as u32
}

/// Borrow the peeked bytes the host wrote at `(ptr, len)` as a `&[u8]`.
///
/// # Safety
/// The caller (a sniffer's own `sniff` export) must pass through exactly the
/// `(in_ptr, in_len)` wasmtime called `sniff` with — a region the host
/// allocated via this module's own [`alloc`] and filled with exactly `len`
/// bytes immediately before the call, per the documented ABI.
pub unsafe fn input<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if len == 0 {
        return &[]; // ptr may be 0 (alloc(0) -> 0); never form a slice from it
    }
    std::slice::from_raw_parts(ptr as *const u8, len as usize)
}

/// Borrow this module's configured `config` string bytes at `(ptr, len)`, or
/// `&[]` when the module has no `settings.sniffers.modules[].config` (the host
/// passes `cfg_len == 0`). Same marshalling as [`input`].
///
/// # Safety
/// As [`input`]: pass through exactly the `(cfg_ptr, cfg_len)` the host called
/// `sniff` with.
pub unsafe fn config<'a>(ptr: u32, len: u32) -> &'a [u8] {
    input(ptr, len)
}

/// A recognised result, mirroring `wayhouse_config::RouteHint`'s three fields.
/// Deliberately does not depend on `wayhouse-config` — a sniffer is a tiny,
/// dependency-free wasm module, not a consumer of the proxy's own crates.
#[derive(Default)]
pub struct Hint<'a> {
    pub host: Option<&'a str>,
    pub key: Option<&'a str>,
    pub reject: bool,
}

/// `sniff`'s return value for "not recognised" — the host treats a literal
/// `0` as a clean miss, never an error.
pub const NOT_RECOGNISED: i64 = 0;

/// Encode `hint` per the compact `RouteHint` format documented above. Pure —
/// no allocation via [`alloc`], no pointer arithmetic — so it's unit tested
/// on any host target; [`emit_hint`] is the thin wrapper that actually places
/// these bytes in linear memory for the host to read back. A host string
/// longer than `u16::MAX` bytes is truncated — hostnames and affinity keys
/// are never that long in practice, and this keeps the encoder infallible.
pub fn encode(hint: &Hint) -> Vec<u8> {
    let mut flags = 0u8;
    if hint.reject {
        flags |= 0b001;
    }
    if hint.host.is_some() {
        flags |= 0b010;
    }
    if hint.key.is_some() {
        flags |= 0b100;
    }

    let mut body = Vec::with_capacity(1 + 34);
    body.push(flags);
    if let Some(h) = hint.host {
        write_string(&mut body, h);
    }
    if let Some(k) = hint.key {
        write_string(&mut body, k);
    }
    body
}

/// [`encode`] `hint`, write it into freshly [`alloc`]-ed linear memory, and
/// return the packed `(ptr << 32) | len` the host expects back from `sniff`.
///
/// Note for anyone tempted to unit-test this directly: `ptr` is a genuine
/// **wasm32** linear-memory offset truncated to `u32` — reading it back via
/// [`input`] is only sound inside the guest itself (or the host's own
/// `wasmtime::Memory`, which is what `crates/wayhouse/src/sniffer_loader.rs`
/// actually does). On a native (non-wasm32) test target a real pointer does
/// not fit in `u32`, so round-tripping through this function would corrupt
/// the address — test [`encode`] instead, and leave the real placement to
/// the wasm-target build the host loader exercises end to end.
pub fn emit_hint(hint: &Hint) -> i64 {
    let body = encode(hint);
    let len = body.len() as u32;
    let ptr = alloc(len);
    if ptr == 0 && len > 0 {
        // Allocation failed; nothing sane to return but "not recognised" —
        // the host will never see a dangling pointer.
        return NOT_RECOGNISED;
    }
    // SAFETY: `ptr` was just allocated by `alloc` for exactly `len` bytes and
    // is not aliased by anything else yet.
    unsafe {
        std::ptr::copy_nonoverlapping(body.as_ptr(), ptr as *mut u8, body.len());
    }
    pack(ptr, len)
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u16::MAX as usize) as u16;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
}

fn pack(ptr: u32, len: u32) -> i64 {
    (((ptr as u64) << 32) | (len as u64)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_bytes_match_constants() {
        let b = ABI_BYTES;
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), ABI_MAJOR);
        assert_eq!(u16::from_le_bytes([b[2], b[3]]), ABI_MINOR);
        assert_eq!((ABI_MAJOR, ABI_MINOR), (0, 1));
    }

    /// Mirrors the host's `decode_route_hint` in
    /// `crates/wayhouse/src/sniffer_loader.rs` — kept independent (not shared code)
    /// since the two sides deliberately only agree on the wire format, not on
    /// a shared crate the guest would need to depend on `wayhouse-core` for.
    fn decode(bytes: &[u8]) -> Option<(Option<String>, Option<String>, bool)> {
        fn read_string(bytes: &[u8], pos: &mut usize) -> Option<String> {
            let len = u16::from_le_bytes(bytes.get(*pos..*pos + 2)?.try_into().ok()?) as usize;
            *pos += 2;
            let s = bytes.get(*pos..*pos + len)?;
            *pos += len;
            std::str::from_utf8(s).ok().map(str::to_string)
        }
        let mut pos = 0usize;
        let flags = *bytes.first()?;
        pos += 1;
        let reject = flags & 0b001 != 0;
        let host = if flags & 0b010 != 0 {
            Some(read_string(bytes, &mut pos)?)
        } else {
            None
        };
        let key = if flags & 0b100 != 0 {
            Some(read_string(bytes, &mut pos)?)
        } else {
            None
        };
        Some((host, key, reject))
    }

    #[test]
    fn encode_round_trips_through_the_wire_format() {
        let bytes = encode(&Hint {
            host: Some("survival.example.net"),
            key: Some("affinity-1"),
            reject: false,
        });
        let (host, key, reject) = decode(&bytes).unwrap();
        assert_eq!(host.as_deref(), Some("survival.example.net"));
        assert_eq!(key.as_deref(), Some("affinity-1"));
        assert!(!reject);
    }

    #[test]
    fn encode_handles_reject_with_no_strings() {
        let bytes = encode(&Hint {
            reject: true,
            ..Default::default()
        });
        assert_eq!(bytes.len(), 1);
        let (host, key, reject) = decode(&bytes).unwrap();
        assert!(host.is_none());
        assert!(key.is_none());
        assert!(reject);
    }

    #[test]
    fn alloc_of_zero_returns_null_and_is_never_dereferenced() {
        assert_eq!(alloc(0), 0);
    }

    /// `emit_hint` / `input` / `pack` are only sound on wasm32 (see
    /// `emit_hint`'s doc note) — the real end-to-end round trip through
    /// `wasmtime::Memory` is exercised by `crates/wayhouse/src/sniffer_loader.rs`'s
    /// tests against compiled `*.wasm` modules, not natively here.
    #[test]
    fn pack_places_len_in_the_low_32_bits() {
        assert_eq!(pack(0, 5) as u64, 5);
        assert_eq!(pack(1, 0) as u64, 1u64 << 32);
    }
}
