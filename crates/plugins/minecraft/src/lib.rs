//! `gsp` sniffer plugin: the "virtual host" a Minecraft client dials.
//!
//! Since protocol 1.7, a Minecraft client's very first packet is a
//! Handshake: `[VarInt length][VarInt packet id = 0x00][VarInt protocol
//! version][String server address][UShort port][VarInt next state]`. The
//! `server address` field is exactly the hostname the client typed in — this
//! is how BungeeCord/Velocity-style proxies do virtual-host routing, and it's
//! exactly what a `sniffer` route's `host:` patterns are for.
//!
//! Recognition is best-effort and read-only: a peek buffer that ends mid-field
//! (TCP `MSG_PEEK` under the proxy's `peek_len()` cap, or a client that split
//! the handshake across segments) is simply "not recognised" — no error, the
//! route just falls through to the next matcher, same as every other sniffer.

/// Read a Minecraft-protocol VarInt (LEB128, up to 5 bytes for an `i32`).
/// Returns `(value, bytes consumed)`.
fn read_varint(buf: &[u8]) -> Option<(i32, usize)> {
    let mut value: i32 = 0;
    for i in 0..5 {
        let byte = *buf.get(i)?;
        value |= i32::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None // more than 5 bytes: malformed
}

/// Parse the handshake and pull out the server-address string. Pure and
/// host-independent so it's unit tested directly; `sniff` below is the thin
/// ABI wrapper the host actually calls.
pub fn recognise(first: &[u8]) -> Option<gsp_sniffer_abi::Hint<'static>> {
    let mut pos = 0usize;

    let (_packet_len, n) = read_varint(&first[pos..])?;
    pos += n;

    let (packet_id, n) = read_varint(&first[pos..])?;
    pos += n;
    if packet_id != 0x00 {
        return None; // not a handshake
    }

    let (_protocol_version, n) = read_varint(&first[pos..])?;
    pos += n;

    let (str_len, n) = read_varint(&first[pos..])?;
    pos += n;
    let str_len = usize::try_from(str_len).ok()?;
    // A real hostname (even with a Forge "\0FML\0..." suffix) fits well
    // inside this bound; a huge claimed length is either garbage or an
    // attempt to make us over-read — reject it rather than allocate for it.
    if str_len == 0 || str_len > 260 {
        return None;
    }
    let host_bytes = first.get(pos..pos + str_len)?;
    pos += str_len;
    let host = std::str::from_utf8(host_bytes).ok()?;
    // Forge clients append "\0FML\0" (or similar) after the real hostname;
    // keep only the part before the first NUL.
    let host = host.split('\0').next().unwrap_or(host);
    if host.is_empty() {
        return None;
    }

    // Port (2 bytes) + next-state VarInt must also be present for this to be
    // a genuine handshake and not a truncated/forged prefix.
    first.get(pos..pos + 2)?;
    pos += 2;
    read_varint(&first[pos..])?;

    // Leaked from the ABI's static-lifetime `Hint` — we hand back an owned
    // String's contents via `Box::leak`, which is fine: this instance (and
    // its whole linear memory) is torn down by the host right after `sniff`
    // returns.
    let host: &'static str = Box::leak(host.to_ascii_lowercase().into_boxed_str());
    Some(gsp_sniffer_abi::Hint {
        host: Some(host),
        ..Default::default()
    })
}

/// # Safety
/// See `gsp_sniffer_abi::input`'s safety note — `ptr`/`len` must be exactly
/// what the host passed to this export.
#[no_mangle]
pub unsafe extern "C" fn sniff(ptr: u32, len: u32) -> i64 {
    let first = gsp_sniffer_abi::input(ptr, len);
    match recognise(first) {
        Some(hint) => gsp_sniffer_abi::emit_hint(&hint),
        None => gsp_sniffer_abi::NOT_RECOGNISED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: i32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (v & 0x7f) as u8;
            v = ((v as u32) >> 7) as i32;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
        out
    }

    fn handshake(protocol_version: i32, address: &str, port: u16, next_state: i32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(varint(0x00)); // packet id
        body.extend(varint(protocol_version));
        body.extend(varint(address.len() as i32));
        body.extend_from_slice(address.as_bytes());
        body.extend_from_slice(&port.to_be_bytes());
        body.extend(varint(next_state));

        let mut pkt = varint(body.len() as i32); // packet length prefix
        pkt.extend(body);
        pkt
    }

    #[test]
    fn extracts_the_server_address() {
        let pkt = handshake(765, "play.example.net", 25565, 1);
        let hint = recognise(&pkt).unwrap();
        assert_eq!(hint.host, Some("play.example.net"));
    }

    #[test]
    fn lowercases_the_host_and_strips_a_forge_suffix() {
        let pkt = handshake(47, "Survival.Example.NET\0FML\0", 25565, 2);
        let hint = recognise(&pkt).unwrap();
        assert_eq!(hint.host, Some("survival.example.net"));
    }

    #[test]
    fn rejects_a_non_handshake_packet_id() {
        let mut body = Vec::new();
        body.extend(varint(0x01)); // not a handshake packet id
        body.extend(varint(0));
        let mut pkt = varint(body.len() as i32);
        pkt.extend(body);
        assert!(recognise(&pkt).is_none());
    }

    #[test]
    fn rejects_truncated_input() {
        let pkt = handshake(765, "play.example.net", 25565, 1);
        // Cut off mid-hostname: must not panic, must just say "no".
        assert!(recognise(&pkt[..pkt.len() - 10]).is_none());
        assert!(recognise(&[]).is_none());
    }

    #[test]
    fn rejects_an_absurd_claimed_string_length() {
        let mut body = Vec::new();
        body.extend(varint(0x00));
        body.extend(varint(765));
        body.extend(varint(100_000)); // way past the 260-byte sanity bound
        let mut pkt = varint(body.len() as i32);
        pkt.extend(body);
        assert!(recognise(&pkt).is_none());
    }
}
