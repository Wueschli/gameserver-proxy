//! Client Initial decryption and SNI extraction (RFC 9001 §5, RFC 9369 §3).
//!
//! The Initial's keys derive from public inputs (the packet's own destination
//! connection ID and a version-specific salt), so a middlebox can read the
//! TLS ClientHello inside. Pure and allocation-light; every failure returns
//! `None` and the caller falls back to the key-only hint.

use aes::cipher::{BlockEncrypt, KeyInit as _};
use aes::Aes128;
use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::{V1, V2};

const SALT_V1: [u8; 20] = [
    0x38, 0x76, 0x2c, 0xf7, 0xf5, 0x59, 0x34, 0xb3, 0x4d, 0x17, 0x9a, 0xe6, 0xa4, 0xc8, 0x0c, 0xad,
    0xcc, 0xbb, 0x7f, 0x0a,
];
const SALT_V2: [u8; 20] = [
    0x0d, 0xed, 0xe3, 0xde, 0xf7, 0x00, 0xa6, 0xdb, 0x81, 0x93, 0x81, 0xbe, 0x6e, 0x26, 0x9d, 0xcb,
    0xf9, 0xbd, 0x2e, 0xd9,
];
/// Drafts 29 to 32 (draft 33 and 34 reuse the v1 salt).
const SALT_DRAFT_29: [u8; 20] = [
    0xaf, 0xbf, 0xec, 0x28, 0x99, 0x93, 0xd2, 0x4c, 0x9e, 0x97, 0x86, 0xf1, 0x9c, 0x61, 0x11, 0xe0,
    0x43, 0x90, 0xa8, 0x99,
];

/// How much CRYPTO data to reassemble. One datagram is at most ~1500 bytes,
/// so this is generous.
const CRYPTO_MAX: usize = 4096;
/// A hostname is at most 253 bytes (RFC 1035).
const HOST_MAX: usize = 253;

struct Keys {
    key: [u8; 16],
    iv: [u8; 12],
    hp: [u8; 16],
}

/// The salt for `version`, or `None` for a draft too old to decrypt.
fn salt(version: u32) -> Option<&'static [u8; 20]> {
    match version {
        V1 | 0xff00_0021 | 0xff00_0022 => Some(&SALT_V1),
        V2 => Some(&SALT_V2),
        0xff00_001d..=0xff00_0020 => Some(&SALT_DRAFT_29),
        _ => None,
    }
}

/// `HKDF-Expand-Label(secret, label, "", out.len())` with the `tls13 ` prefix.
fn expand_label(hk: &Hkdf<Sha256>, label: &str, out: &mut [u8]) -> Option<()> {
    let mut info = [0u8; 2 + 1 + 6 + 16 + 1];
    let full = 6 + label.len();
    info[..2].copy_from_slice(&(out.len() as u16).to_be_bytes());
    info[2] = full as u8;
    info[3..9].copy_from_slice(b"tls13 ");
    info[9..9 + label.len()].copy_from_slice(label.as_bytes());
    // empty context: length byte 0
    hk.expand(&info[..3 + full + 1], out).ok()
}

fn client_keys(version: u32, dcid: &[u8]) -> Option<Keys> {
    let salt = salt(version)?;
    let (_, initial) = Hkdf::<Sha256>::extract(Some(salt), dcid);
    let mut client = [0u8; 32];
    expand_label(&initial, "client in", &mut client)?;
    let hk = Hkdf::<Sha256>::from_prk(&client).ok()?;
    let (k, i, h) = if version == V2 {
        ("quicv2 key", "quicv2 iv", "quicv2 hp")
    } else {
        ("quic key", "quic iv", "quic hp")
    };
    let mut keys = Keys {
        key: [0; 16],
        iv: [0; 12],
        hp: [0; 16],
    };
    expand_label(&hk, k, &mut keys.key)?;
    expand_label(&hk, i, &mut keys.iv)?;
    expand_label(&hk, h, &mut keys.hp)?;
    Some(keys)
}

/// QUIC variable-length integer (RFC 9000 §16): returns the value and the
/// number of bytes consumed.
fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    let len = 1usize << (first >> 6);
    let bytes = b.get(..len)?;
    let mut v = u64::from(first & 0x3f);
    for &x in &bytes[1..] {
        v = (v << 8) | u64::from(x);
    }
    Some((v, len))
}

/// Decrypt the client Initial packet at the start of `pkt` and return the SNI
/// from the ClientHello it carries, lower-cased.
pub fn extract_sni(pkt: &[u8]) -> Option<String> {
    let version = u32::from_be_bytes(pkt.get(1..5)?.try_into().ok()?);
    let dcid_len = usize::from(*pkt.get(5)?);
    let dcid = pkt.get(6..6 + dcid_len)?;
    let mut pos = 6 + dcid_len;
    let scid_len = usize::from(*pkt.get(pos)?);
    pos += 1 + scid_len;
    let (token_len, n) = varint(pkt.get(pos..)?)?;
    pos = pos
        .checked_add(n)?
        .checked_add(usize::try_from(token_len).ok()?)?;
    let (length, n) = varint(pkt.get(pos..)?)?;
    pos += n;
    let pn_offset = pos;
    let length = usize::try_from(length).ok()?;
    // `length` covers the packet number and the protected payload; a
    // datagram cut short of it cannot be authenticated.
    let end = pn_offset.checked_add(length)?;
    let packet = pkt.get(..end)?;
    // Header protection sample: 16 bytes starting 4 bytes after the PN start.
    let sample = packet.get(pn_offset + 4..pn_offset + 20)?;

    let keys = client_keys(version, dcid)?;
    let mut mask = aes::Block::clone_from_slice(sample);
    Aes128::new(&keys.hp.into()).encrypt_block(&mut mask);

    let b0 = packet[0] ^ (mask[0] & 0x0f);
    let pn_len = usize::from(b0 & 0x03) + 1;
    if length < pn_len + 16 {
        return None;
    }
    let mut header = [0u8; 1500];
    let header_len = pn_offset + pn_len;
    header
        .get_mut(..header_len)?
        .copy_from_slice(&packet[..header_len]);
    header[0] = b0;
    let mut pn = 0u64;
    for i in 0..pn_len {
        header[pn_offset + i] ^= mask[1 + i];
        pn = (pn << 8) | u64::from(header[pn_offset + i]);
    }

    let mut nonce = keys.iv;
    for (n, p) in nonce[4..].iter_mut().zip(pn.to_be_bytes()) {
        *n ^= p;
    }
    let ct_end = packet.len() - 16;
    let mut payload = packet.get(header_len..ct_end)?.to_vec();
    let tag = Tag::clone_from_slice(&packet[ct_end..]);
    Aes128Gcm::new(&keys.key.into())
        .decrypt_in_place_detached(
            Nonce::from_slice(&nonce),
            &header[..header_len],
            &mut payload,
            &tag,
        )
        .ok()?;

    let crypto = crypto_stream(&payload)?;
    server_name(&crypto)
}

/// Reassemble the CRYPTO frames of one packet into the contiguous prefix of
/// the handshake stream.
fn crypto_stream(frames: &[u8]) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; CRYPTO_MAX];
    let mut have = vec![false; CRYPTO_MAX];
    let mut rest = frames;
    while !rest.is_empty() {
        let (ty, n) = varint(rest)?;
        rest = &rest[n..];
        match ty {
            0x00 | 0x01 => {} // PADDING, PING
            0x02 | 0x03 => {
                // ACK: largest, delay, range count, first range, then ranges.
                let (_, n) = varint(rest)?;
                rest = &rest[n..];
                let (_, n) = varint(rest)?;
                rest = &rest[n..];
                let (ranges, n) = varint(rest)?;
                rest = &rest[n..];
                let (_, n) = varint(rest)?;
                rest = &rest[n..];
                for _ in 0..ranges {
                    for _ in 0..2 {
                        let (_, n) = varint(rest)?;
                        rest = &rest[n..];
                    }
                }
                if ty == 0x03 {
                    for _ in 0..3 {
                        let (_, n) = varint(rest)?;
                        rest = &rest[n..];
                    }
                }
            }
            0x06 => {
                let (off, n) = varint(rest)?;
                rest = &rest[n..];
                let (len, n) = varint(rest)?;
                rest = &rest[n..];
                let (off, len) = (usize::try_from(off).ok()?, usize::try_from(len).ok()?);
                let data = rest.get(..len)?;
                rest = &rest[len..];
                let end = off.checked_add(len)?;
                if end > CRYPTO_MAX {
                    return None;
                }
                buf[off..end].copy_from_slice(data);
                have[off..end].fill(true);
            }
            _ => break, // CONNECTION_CLOSE or anything else: stop, use what we have
        }
    }
    let prefix = have.iter().position(|h| !h).unwrap_or(CRYPTO_MAX);
    buf.truncate(prefix);
    Some(buf)
}

/// Walk a (possibly truncated) ClientHello for the `server_name` extension.
/// The handshake length is not checked against the buffer: a ClientHello that
/// spans several Initial packets is still readable if the SNI sits in the part
/// this packet carries.
fn server_name(hs: &[u8]) -> Option<String> {
    struct R<'a>(&'a [u8]);
    impl<'a> R<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
            self.0 = b;
            Some(a)
        }
        fn u8(&mut self) -> Option<usize> {
            Some(usize::from(self.take(1)?[0]))
        }
        fn u16(&mut self) -> Option<usize> {
            let b = self.take(2)?;
            Some(usize::from(u16::from_be_bytes([b[0], b[1]])))
        }
    }
    let mut r = R(hs);
    if r.u8()? != 1 {
        return None; // not a ClientHello
    }
    r.take(3)?; // handshake length
    r.take(2 + 32)?; // legacy_version, random
    let n = r.u8()?;
    r.take(n)?; // session id
    let n = r.u16()?;
    r.take(n)?; // cipher suites
    let n = r.u8()?;
    r.take(n)?; // compression methods
    r.u16()?; // extensions length (may exceed what we hold)
    while r.0.len() >= 4 {
        let ty = r.u16()?;
        let len = r.u16()?;
        if ty != 0 {
            r.take(len)?;
            continue;
        }
        let mut ext = R(r.take(len)?);
        let list = ext.u16()?;
        let mut list = R(ext.take(list)?);
        while !list.0.is_empty() {
            let name_type = list.u8()?;
            let n = list.u16()?;
            let name = list.take(n)?;
            if name_type == 0 {
                return clean_host(name);
            }
        }
        return None;
    }
    None
}

/// Lower-case the name and refuse anything that is not a plausible hostname.
fn clean_host(name: &[u8]) -> Option<String> {
    if name.is_empty() || name.len() > HOST_MAX {
        return None;
    }
    if !name
        .iter()
        .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
    {
        return None;
    }
    Some(String::from_utf8_lossy(name).to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn derives_the_rfc_9001_a1_client_keys() {
        let k = client_keys(V1, &hex("8394c8f03e515708")).unwrap();
        assert_eq!(k.key, *hex("1f369613dd76d5467730efcbe3b1a22d"));
        assert_eq!(k.iv, *hex("fa044b2f42a3fd3b46fb255c"));
        assert_eq!(k.hp, *hex("9f50449e04a0e810283a1e9933adedd2"));
    }

    #[test]
    fn derives_the_rfc_9369_a1_client_keys() {
        let k = client_keys(V2, &hex("8394c8f03e515708")).unwrap();
        assert_eq!(k.key, *hex("8b1a0bc121284290a29e0971b5cd045d"));
        assert_eq!(k.iv, *hex("91f73e2351d8fa91660e909f"));
    }

    // Real client Initials produced by aioquic (an independent QUIC stack).
    #[test]
    fn extracts_the_sni_from_a_real_v1_initial() {
        let p = hex(include_str!("../testdata/v1_example_com.hex"));
        assert_eq!(extract_sni(&p).as_deref(), Some("example.com"));
    }

    #[test]
    fn lowercases_the_sni() {
        let p = hex(include_str!("../testdata/v1_mixed_case.hex"));
        assert_eq!(extract_sni(&p).as_deref(), Some("play.example.net"));
    }

    #[test]
    fn extracts_the_sni_from_a_real_v2_initial() {
        let p = hex(include_str!("../testdata/v2_example_com.hex"));
        assert_eq!(extract_sni(&p).as_deref(), Some("example.com"));
    }

    #[test]
    fn a_tampered_or_truncated_packet_yields_nothing() {
        let p = hex(include_str!("../testdata/v1_example_com.hex"));
        let mut bad = p.clone();
        bad[300] ^= 1;
        assert_eq!(extract_sni(&bad), None); // AEAD tag fails
                                             // Anything shorter than the packet's declared length cannot be
                                             // authenticated; bytes after it (coalesced packets, padding) are ignored.
        let end = 511; // 1+4+(1+8)+(1+8)+1+2 header bytes, then Length = 0x1e5 = 485
        assert!(extract_sni(&p[..end]).is_some());
        for cut in [0, 1, 5, 20, 100, 400, end - 1] {
            assert_eq!(extract_sni(&p[..cut]), None, "cut at {cut}");
        }
    }

    #[test]
    fn parses_an_sni_from_a_truncated_client_hello() {
        // 1, len, version, random, empty sid, one suite, null compression,
        // then extensions: supported_groups, server_name, and a cut-off one.
        let mut hs = vec![1, 0, 0, 0xff, 3, 3];
        hs.extend([0u8; 32]);
        hs.extend([0, 0, 2, 0x13, 0x01, 1, 0, 0xff, 0xff]);
        hs.extend([0, 10, 0, 4, 0, 2, 0, 29]);
        hs.extend([0, 0, 0, 14, 0, 12, 0, 0, 9]);
        hs.extend(b"Host.Test");
        hs.extend([0, 51, 4, 0, 1, 2]); // key_share cut off mid-extension
        assert_eq!(server_name(&hs).as_deref(), Some("host.test"));
    }

    #[test]
    fn rejects_non_hostnames_and_non_client_hellos() {
        assert_eq!(clean_host(b""), None);
        assert_eq!(clean_host(b"bad host"), None);
        assert_eq!(clean_host(&[b'a'; 254]), None);
        assert_eq!(server_name(&[2, 0, 0, 0]), None);
        assert_eq!(server_name(&[]), None);
    }

    #[test]
    fn old_drafts_have_no_salt() {
        assert!(salt(0xff00_0000 + 28).is_none());
        assert!(salt(0xff00_001d).is_some());
    }
}
