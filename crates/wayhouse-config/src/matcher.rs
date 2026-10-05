//! Route matchers, the match context, and TLS SNI extraction.

use std::net::SocketAddr;
use std::ops::RangeInclusive;

use crate::cidr::Cidr;

/// A host pattern for the `sni` matcher. Parsed lowercase; `*.foo` and `.foo`
/// both become `Suffix(".foo")` (a proper-subdomain match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    Exact(String),
    /// Includes the leading `.`; matches `host.ends_with(self)`.
    Suffix(String),
}

impl HostPattern {
    fn matches(&self, host: &str) -> bool {
        match self {
            HostPattern::Exact(e) => host == e,
            HostPattern::Suffix(s) => host.ends_with(s.as_str()),
        }
    }
}

/// Structured hints a sniffer plugin returns after inspecting a connection's
/// first bytes. Read-only: a sniffer never sees later bytes and never writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteHint {
    /// A hostname the sniffer recognised (Minecraft handshake host, TLS SNI, …).
    pub host: Option<String>,
    /// An opaque affinity / routing key (fed to a sticky table later).
    pub key: Option<String>,
    /// The sniffer wants this connection / datagram rejected outright. `wayhouse-core`
    /// drops it before routing (TCP: `wayhouse_listener_connections_total{result=
    /// "sniffer_reject"}`; UDP: no session, no reply,
    /// `wayhouse_datagrams_dropped_total{reason="sniffer_reject"}`) — it does not fall
    /// through to a later route such as `always`.
    pub reject: bool,
}

/// A single route's match condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Catch-all.
    Always,
    /// Source IP within any of these prefixes.
    ClientCidr(Vec<Cidr>),
    /// Destination IP (the address the client connected to, from `getsockname` /
    /// the listener's bind) within any of these prefixes. Only distinguishes
    /// addresses the OS already delivers separately; a wildcard-prefix listener
    /// (`IP_PKTINFO`) that makes this useful for a whole routed prefix is a
    /// later slice.
    DstCidr(Vec<Cidr>),
    /// Destination port (from the accepting socket) within any of these ranges.
    DstPort(Vec<RangeInclusive<u16>>),
    /// The connection's first bytes (TCP peek / first UDP datagram): start with
    /// `prefix` (when non-empty) **and** have an observed length within `len`
    /// (when set). At least one condition is present. On TCP `len` sees only
    /// what one peek returned (coarse); on UDP it is the exact datagram length.
    /// `regex` / `sniffer` variants come later.
    FirstBytes {
        prefix: Vec<u8>,
        len: Option<RangeInclusive<usize>>,
    },
    /// The TLS ClientHello's SNI host matches one of these patterns (TCP only;
    /// TLS is peeked, not terminated).
    Sni(Vec<HostPattern>),
    /// The named sniffer plugin (see `wayhouse_core::sniff`) recognised the first
    /// bytes. With `host` patterns: also requires the hint's `host` to match
    /// one of them; empty `host` ⇒ matches on any recognition. A listener may
    /// use several sniffers; the first to recognise the bytes wins, and this
    /// matcher only matches that winner by name (see
    /// [`ListenerConfig::sniffers`]).
    ///
    /// A `reject` hint never reaches this matcher — `wayhouse-core` drops the
    /// connection / datagram before routing (see [`RouteHint::reject`]) — so
    /// the `!reject` guard below is belt-and-braces for a direct `route_for`
    /// caller. The name is not validated here — the proxy checks it against
    /// the loaded sniffers at listener start (an unknown name simply never
    /// matches).
    Sniffer {
        name: String,
        host: Vec<HostPattern>,
    },
}

/// Hard cap on how many leading bytes the TCP path will `MSG_PEEK` (and on the
/// size of the peek buffer). Large enough for a typical TLS ClientHello.
pub const PEEK_MAX: usize = 4096;

/// Hard cap on a `first_bytes` prefix length.
pub(crate) const FIRST_BYTES_PREFIX_MAX: usize = 512;

/// Everything a [`Matcher`] can look at. `first_bytes` is empty when the listener
/// has no byte matcher (so nothing was peeked) or the peer sent nothing yet.
pub struct MatchContext<'a> {
    pub src: SocketAddr,
    pub local: SocketAddr,
    pub first_bytes: &'a [u8],
    /// The listener's sniffer result, if it has `sniffer` routes and one of
    /// its plugins recognised the bytes: the plugin's configured name and its
    /// hint. A `sniffer` route only matches the plugin it names. `wayhouse-core`
    /// fills this in before routing.
    pub sniff: Option<(&'a str, &'a RouteHint)>,
}

impl Matcher {
    pub fn matches(&self, ctx: &MatchContext) -> bool {
        match self {
            Matcher::Always => true,
            Matcher::ClientCidr(cidrs) => cidrs.iter().any(|c| c.contains(ctx.src.ip())),
            Matcher::DstCidr(cidrs) => cidrs.iter().any(|c| c.contains(ctx.local.ip())),
            Matcher::DstPort(ranges) => ranges.iter().any(|r| r.contains(&ctx.local.port())),
            Matcher::FirstBytes { prefix, len } => {
                let b = ctx.first_bytes;
                (prefix.is_empty() || b.starts_with(prefix.as_slice()))
                    && len.as_ref().is_none_or(|r| r.contains(&b.len()))
            }
            Matcher::Sni(pats) => match extract_sni(ctx.first_bytes) {
                Some(host) => pats.iter().any(|p| p.matches(&host)),
                None => false,
            },
            Matcher::Sniffer { name, host } => match ctx.sniff {
                Some((hit, hint)) if hit == name && !hint.reject => {
                    host.is_empty()
                        || hint
                            .host
                            .as_deref()
                            .is_some_and(|h| host.iter().any(|p| p.matches(h)))
                }
                _ => false,
            },
        }
    }

    /// Leading bytes this matcher needs to see (0 for address-only matchers).
    pub(crate) fn peek_len(&self) -> usize {
        match self {
            // For a length bound, one byte past the upper end is enough to tell
            // "within range" from "above range".
            Matcher::FirstBytes { prefix, len } => prefix.len().max(
                len.as_ref()
                    .map_or(0, |r| r.end().saturating_add(1).min(PEEK_MAX)),
            ),
            Matcher::Sni(_) | Matcher::Sniffer { .. } => PEEK_MAX,
            _ => 0,
        }
    }
}

/// Extract the SNI `host_name` from a TLS ClientHello at the start of `buf`.
/// Returns `None` if `buf` is not a ClientHello, is truncated, or carries no
/// SNI. The TCP listener reassembles a ClientHello split across TCP segments
/// before calling this (it re-peeks until the whole first TLS record is
/// buffered or the peek budget expires), so a `None` from a genuine truncation
/// means the client stalled mid-handshake past the budget — that route then
/// just does not match.
pub fn extract_sni(buf: &[u8]) -> Option<String> {
    struct Reader<'a> {
        b: &'a [u8],
        pos: usize,
    }
    impl<'a> Reader<'a> {
        fn new(b: &'a [u8]) -> Self {
            Self { b, pos: 0 }
        }
        fn remaining(&self) -> usize {
            self.b.len() - self.pos
        }
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let end = self.pos.checked_add(n)?;
            let s = self.b.get(self.pos..end)?;
            self.pos = end;
            Some(s)
        }
        fn u8(&mut self) -> Option<u8> {
            Some(self.take(1)?[0])
        }
        fn u16(&mut self) -> Option<usize> {
            let x = self.take(2)?;
            Some(u16::from_be_bytes([x[0], x[1]]) as usize)
        }
        fn u24(&mut self) -> Option<usize> {
            let x = self.take(3)?;
            Some(u32::from_be_bytes([0, x[0], x[1], x[2]]) as usize)
        }
    }

    let mut r = Reader::new(buf);
    if r.u8()? != 0x16 {
        return None; // not a handshake record
    }
    r.take(2)?; // record version
    let rec_len = r.u16()?;
    let rec = r.take(rec_len)?; // first record fragment

    let mut h = Reader::new(rec);
    if h.u8()? != 0x01 {
        return None; // not a ClientHello
    }
    let hs_len = h.u24()?;
    let body = h.take(hs_len)?;

    let mut c = Reader::new(body);
    c.take(2)?; // client_version
    c.take(32)?; // random
    let sid_len = c.u8()? as usize;
    c.take(sid_len)?;
    let cs_len = c.u16()?;
    c.take(cs_len)?;
    let comp_len = c.u8()? as usize;
    c.take(comp_len)?;
    let ext_total = c.u16()?;
    let exts = c.take(ext_total)?;

    let mut e = Reader::new(exts);
    while e.remaining() >= 4 {
        let ext_type = e.u16()?;
        let ext_len = e.u16()?;
        let ext_data = e.take(ext_len)?;
        if ext_type != 0x0000 {
            continue; // not server_name
        }
        let mut s = Reader::new(ext_data);
        let list_len = s.u16()?;
        let list = s.take(list_len)?;
        let mut l = Reader::new(list);
        while l.remaining() >= 3 {
            let name_type = l.u8()?;
            let name_len = l.u16()?;
            let name = l.take(name_len)?;
            if name_type == 0x00 {
                let host = std::str::from_utf8(name).ok()?.to_ascii_lowercase();
                return (!host.is_empty()).then_some(host);
            }
        }
        return None;
    }
    None
}
