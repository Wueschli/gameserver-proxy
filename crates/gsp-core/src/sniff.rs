//! In-process, static **sniffer plugins**.
//!
//! A sniffer takes a read-only look at a connection's first bytes (TCP peek /
//! first UDP datagram) and, if it recognises the protocol, returns a
//! [`RouteHint`] — a hostname, an affinity key, or a reject flag. The `sniffer`
//! route matcher then compares that hint. Sniffers never see later bytes and
//! never write: game-specific parsing lives here and only here, off the
//! forwarding path (see `docs/03`, `CLAUDE.md` "agnostic core").
//!
//! The set here must stay in sync with `gsp_config::KNOWN_SNIFFERS` (which
//! `validate()` uses to reject unknown names).

use gsp_config::{extract_sni, RouteHint};

/// A read-only first-bytes inspector.
pub trait Sniffer: Send + Sync {
    fn name(&self) -> &'static str;
    /// Inspect the (bounded) first bytes. `None` = not recognised.
    fn sniff(&self, first: &[u8]) -> Option<RouteHint>;
}

/// Look up a sniffer by its config name.
pub fn sniffer(name: &str) -> Option<&'static dyn Sniffer> {
    match name {
        "sni" => Some(&Sni),
        "minecraft" => Some(&Minecraft),
        "a2s" => Some(&A2s),
        _ => None,
    }
}

/// TLS SNI — reuses the ClientHello reader from `gsp-config`.
struct Sni;
impl Sniffer for Sni {
    fn name(&self) -> &'static str {
        "sni"
    }
    fn sniff(&self, first: &[u8]) -> Option<RouteHint> {
        extract_sni(first).map(|host| RouteHint {
            host: Some(host),
            ..Default::default()
        })
    }
}

/// Valve A2S / Source query: the connectionless `0xFFFFFFFF` header. Recognises
/// a query packet (no host); route it to a dedicated query pool.
struct A2s;
impl Sniffer for A2s {
    fn name(&self) -> &'static str {
        "a2s"
    }
    fn sniff(&self, first: &[u8]) -> Option<RouteHint> {
        first
            .starts_with(&[0xff, 0xff, 0xff, 0xff])
            .then(|| RouteHint {
                key: Some("a2s".into()),
                ..Default::default()
            })
    }
}

/// Minecraft (Java) — the uncompressed pre-login Handshake packet:
/// `len:VarInt id:VarInt(=0) proto:VarInt addr:(VarInt len + UTF-8) port:u16
/// next_state:VarInt`. Returns the `addr` string as the host.
struct Minecraft;
impl Sniffer for Minecraft {
    fn name(&self) -> &'static str {
        "minecraft"
    }
    fn sniff(&self, first: &[u8]) -> Option<RouteHint> {
        let mut r = first;
        let pkt_len = read_varint(&mut r)? as usize;
        // Keep parsing within the declared packet (best effort if truncated).
        let end = pkt_len.min(r.len());
        let mut body = &r[..end];
        if read_varint(&mut body)? != 0x00 {
            return None; // not a Handshake packet
        }
        read_varint(&mut body)?; // protocol version
        let host_len = read_varint(&mut body)? as usize;
        if host_len == 0 || host_len > body.len() || host_len > 255 {
            return None;
        }
        let host = std::str::from_utf8(&body[..host_len]).ok()?;
        Some(RouteHint {
            host: Some(host.to_ascii_lowercase()),
            ..Default::default()
        })
    }
}

/// Read a Minecraft VarInt (LEB128-ish, ≤ 5 bytes) from the front of `r`,
/// advancing it. `None` on truncation or overlong encoding.
fn read_varint(r: &mut &[u8]) -> Option<i32> {
    let mut result: i32 = 0;
    for i in 0..5 {
        let (&byte, rest) = r.split_first()?;
        *r = rest;
        result |= ((byte & 0x7f) as i32) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(result);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2s_recognises_the_connectionless_header() {
        assert!(A2s.sniff(&[0xff, 0xff, 0xff, 0xff, 0x54]).is_some());
        assert!(A2s.sniff(b"\x01\x02\x03\x04").is_none());
    }

    #[test]
    fn sni_sniffer_extracts_the_host() {
        // Minimal ClientHello with SNI "eu.example.com".
        let sni = "eu.example.com";
        let mut sn = Vec::new();
        sn.extend_from_slice(&((sni.len() + 3) as u16).to_be_bytes());
        sn.push(0);
        sn.extend_from_slice(&(sni.len() as u16).to_be_bytes());
        sn.extend_from_slice(sni.as_bytes());
        let mut ext = Vec::new();
        ext.extend_from_slice(&0u16.to_be_bytes());
        ext.extend_from_slice(&(sn.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sn);
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x2f]);
        body.push(1);
        body.push(0);
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);
        let bl = body.len();
        let mut hs = vec![0x01, (bl >> 16) as u8, (bl >> 8) as u8, bl as u8];
        hs.extend_from_slice(&body);
        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);

        assert_eq!(
            Sni.sniff(&rec).unwrap().host.as_deref(),
            Some("eu.example.com")
        );
        assert!(Sni.sniff(b"not tls").is_none());
    }

    /// Build a Minecraft Handshake packet for `host`.
    fn mc_handshake(host: &str) -> Vec<u8> {
        fn varint(mut v: u32, out: &mut Vec<u8>) {
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    break;
                }
                out.push(b | 0x80);
            }
        }
        let mut body = Vec::new();
        varint(0x00, &mut body); // packet id
        varint(765, &mut body); // protocol version
        varint(host.len() as u32, &mut body);
        body.extend_from_slice(host.as_bytes());
        body.extend_from_slice(&25565u16.to_be_bytes());
        varint(2, &mut body); // next state = login
        let mut pkt = Vec::new();
        varint(body.len() as u32, &mut pkt);
        pkt.extend_from_slice(&body);
        pkt
    }

    #[test]
    fn minecraft_sniffer_reads_the_handshake_host() {
        let pkt = mc_handshake("Survival.Example.NET");
        assert_eq!(
            Minecraft.sniff(&pkt).unwrap().host.as_deref(),
            Some("survival.example.net")
        );
        assert!(Minecraft.sniff(b"\xfe\x01").is_none()); // legacy ping
        assert!(Minecraft.sniff(&pkt[..3]).is_none()); // truncated
    }

    #[test]
    fn registry_maps_known_names() {
        for n in gsp_config::KNOWN_SNIFFERS {
            assert_eq!(sniffer(n).unwrap().name(), *n);
        }
        assert!(sniffer("nope").is_none());
    }
}
