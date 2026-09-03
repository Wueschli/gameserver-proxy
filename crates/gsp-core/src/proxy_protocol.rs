//! [PROXY protocol] header encoding.
//!
//! When a pool sets `proxy_protocol: v1 | v2`, the proxy prepends one header to
//! the upstream connection before any client payload so the backend learns the
//! real client address. We only ever *write* headers (the proxy is the sender);
//! parsing is the backend's job.
//!
//! v1 is the human-readable text form (`PROXY TCP4 src dst sport dport\r\n`);
//! v2 is the binary form. Both describe a single hop: `src` = the connecting
//! client, `dst` = the address the client connected to on this proxy.
//!
//! If `src` and `dst` are somehow different address families (should not happen
//! for an accepted TCP connection) we fall back to the "unknown connection"
//! encoding, which carries no addresses.
//!
//! [PROXY protocol]: https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt

use std::net::SocketAddr;

use gsp_config::ProxyProtocol;

const V2_SIG: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// Encode the header for `mode`. `mode == None` yields an empty vec (nothing to
/// send). `stream` selects the transport byte in v2 (TCP vs UDP).
pub fn header(mode: ProxyProtocol, src: SocketAddr, dst: SocketAddr, stream: bool) -> Vec<u8> {
    match mode {
        ProxyProtocol::None => Vec::new(),
        ProxyProtocol::V1 => v1(src, dst).into_bytes(),
        ProxyProtocol::V2 => v2(src, dst, stream),
    }
}

fn v1(src: SocketAddr, dst: SocketAddr) -> String {
    match (src, dst) {
        (SocketAddr::V4(s), SocketAddr::V4(d)) => format!(
            "PROXY TCP4 {} {} {} {}\r\n",
            s.ip(),
            d.ip(),
            s.port(),
            d.port()
        ),
        (SocketAddr::V6(s), SocketAddr::V6(d)) => format!(
            "PROXY TCP6 {} {} {} {}\r\n",
            s.ip(),
            d.ip(),
            s.port(),
            d.port()
        ),
        _ => "PROXY UNKNOWN\r\n".to_string(),
    }
}

fn v2(src: SocketAddr, dst: SocketAddr, stream: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(52);
    out.extend_from_slice(&V2_SIG);
    // version 2 (0x20) + command PROXY (0x01); LOCAL (0x00) for the fallback.
    let transport = if stream { 0x1 } else { 0x2 };
    match (src, dst) {
        (SocketAddr::V4(s), SocketAddr::V4(d)) => {
            out.push(0x21);
            out.push(0x10 | transport); // AF_INET
            out.extend_from_slice(&12u16.to_be_bytes());
            out.extend_from_slice(&s.ip().octets());
            out.extend_from_slice(&d.ip().octets());
            out.extend_from_slice(&s.port().to_be_bytes());
            out.extend_from_slice(&d.port().to_be_bytes());
        }
        (SocketAddr::V6(s), SocketAddr::V6(d)) => {
            out.push(0x21);
            out.push(0x20 | transport); // AF_INET6
            out.extend_from_slice(&36u16.to_be_bytes());
            out.extend_from_slice(&s.ip().octets());
            out.extend_from_slice(&d.ip().octets());
            out.extend_from_slice(&s.port().to_be_bytes());
            out.extend_from_slice(&d.port().to_be_bytes());
        }
        _ => {
            out.push(0x20); // LOCAL: backend uses the real socket addresses
            out.push(0x00); // AF_UNSPEC
            out.extend_from_slice(&0u16.to_be_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_ipv4_text() {
        let h = header(
            ProxyProtocol::V1,
            "192.0.2.1:56324".parse().unwrap(),
            "198.51.100.7:443".parse().unwrap(),
            true,
        );
        assert_eq!(h, b"PROXY TCP4 192.0.2.1 198.51.100.7 56324 443\r\n");
    }

    #[test]
    fn v1_ipv6_text() {
        let h = header(
            ProxyProtocol::V1,
            "[2001:db8::1]:8080".parse().unwrap(),
            "[2001:db8::2]:9090".parse().unwrap(),
            true,
        );
        assert_eq!(h, b"PROXY TCP6 2001:db8::1 2001:db8::2 8080 9090\r\n");
    }

    #[test]
    fn none_is_empty() {
        assert!(header(
            ProxyProtocol::None,
            "192.0.2.1:1".parse().unwrap(),
            "192.0.2.2:2".parse().unwrap(),
            true
        )
        .is_empty());
    }

    #[test]
    fn v2_ipv4_binary_layout() {
        let h = header(
            ProxyProtocol::V2,
            "192.0.2.1:56324".parse().unwrap(),
            "198.51.100.7:443".parse().unwrap(),
            true,
        );
        assert_eq!(&h[..12], &V2_SIG);
        assert_eq!(h[12], 0x21);
        assert_eq!(h[13], 0x11); // AF_INET + STREAM
        assert_eq!(&h[14..16], &12u16.to_be_bytes());
        assert_eq!(&h[16..20], &[192, 0, 2, 1]);
        assert_eq!(&h[20..24], &[198, 51, 100, 7]);
        assert_eq!(&h[24..26], &56324u16.to_be_bytes());
        assert_eq!(&h[26..28], &443u16.to_be_bytes());
        assert_eq!(h.len(), 28);
    }

    #[test]
    fn v2_udp_transport_byte() {
        let h = header(
            ProxyProtocol::V2,
            "192.0.2.1:1".parse().unwrap(),
            "192.0.2.2:2".parse().unwrap(),
            false,
        );
        assert_eq!(h[13], 0x12); // AF_INET + DGRAM
    }

    #[test]
    fn v2_mixed_family_falls_back_to_local() {
        let h = header(
            ProxyProtocol::V2,
            "192.0.2.1:1".parse().unwrap(),
            "[2001:db8::2]:2".parse().unwrap(),
            true,
        );
        assert_eq!(h[12], 0x20); // LOCAL
        assert_eq!(h[13], 0x00); // AF_UNSPEC
        assert_eq!(&h[14..16], &0u16.to_be_bytes());
        assert_eq!(h.len(), 16);
    }
}
