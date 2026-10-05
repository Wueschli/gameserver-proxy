//! Parsing helpers for individual config values (matchers, bind specs, hex, …).

use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;

use crate::cidr::Cidr;
use crate::matcher::{HostPattern, Matcher, FIRST_BYTES_PREFIX_MAX, PEEK_MAX};
use crate::resolved::*;
use crate::schema::*;
use crate::ConfigError;

pub(crate) fn parse_matcher(lname: &str, i: usize, m: &RawMatch) -> Result<Matcher, ConfigError> {
    use ConfigError::Invalid;
    let at = |s: String| Invalid(format!("listener {lname}: route {i}: {s}"));

    // Reject fields that do not belong to this match type.
    let allow = |field: &str, present: bool, allowed: bool| {
        if present && !allowed {
            Err(at(format!(
                "match type `{}` does not take `{field}`",
                m.kind
            )))
        } else {
            Ok(())
        }
    };
    allow(
        "cidrs",
        m.cidrs.is_some(),
        m.kind == "client_cidr" || m.kind == "dst",
    )?;
    allow("ports", m.ports.is_some(), m.kind == "port")?;
    allow("prefix", m.prefix.is_some(), m.kind == "first_bytes")?;
    allow("length", m.length.is_some(), m.kind == "first_bytes")?;
    allow(
        "host",
        m.host.is_some(),
        m.kind == "sni" || m.kind == "sniffer",
    )?;
    allow("sniffer", m.sniffer.is_some(), m.kind == "sniffer")?;

    // Optional (present or empty) host-pattern list, shared by `sni` / `sniffer`.
    let host_patterns = |min_one: bool| -> Result<Vec<HostPattern>, ConfigError> {
        match m.host.as_ref().filter(|h| !h.is_empty()) {
            Some(raw) => raw
                .iter()
                .map(|h| parse_host_pattern(h).map_err(at))
                .collect(),
            None if min_one => Err(at(format!(
                "match type `{}` needs a non-empty `host` list",
                m.kind
            ))),
            None => Ok(Vec::new()),
        }
    };

    match m.kind.as_str() {
        "always" => Ok(Matcher::Always),
        "client_cidr" | "dst" => {
            let raw = m.cidrs.as_ref().filter(|c| !c.is_empty()).ok_or_else(|| {
                at(format!(
                    "match type `{}` needs a non-empty `cidrs` list",
                    m.kind
                ))
            })?;
            let mut cidrs = Vec::with_capacity(raw.len());
            for c in raw {
                cidrs.push(Cidr::parse(c).map_err(at)?);
            }
            Ok(if m.kind == "dst" {
                Matcher::DstCidr(cidrs)
            } else {
                Matcher::ClientCidr(cidrs)
            })
        }
        "port" => {
            let raw = m
                .ports
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| at("match type `port` needs a non-empty `ports` list".into()))?;
            let mut ranges = Vec::with_capacity(raw.len());
            for p in raw {
                ranges.push(parse_port_range(lname, i, p)?);
            }
            Ok(Matcher::DstPort(ranges))
        }
        "first_bytes" => {
            let prefix = match &m.prefix {
                Some(s) => parse_byte_spec(s).map_err(at)?,
                None => Vec::new(),
            };
            if prefix.len() > FIRST_BYTES_PREFIX_MAX {
                return Err(at(format!(
                    "`first_bytes` prefix is {} bytes, over the {FIRST_BYTES_PREFIX_MAX}-byte limit",
                    prefix.len()
                )));
            }
            let len = match &m.length {
                Some(l) => {
                    if l.min > l.max {
                        return Err(at(format!(
                            "`first_bytes` length min {} is greater than max {}",
                            l.min, l.max
                        )));
                    }
                    Some(l.min..=l.max)
                }
                None => None,
            };
            if prefix.is_empty() && len.is_none() {
                return Err(at(
                    "match type `first_bytes` needs a `prefix` and/or a `length`".into(),
                ));
            }
            Ok(Matcher::FirstBytes { prefix, len })
        }
        "sni" => Ok(Matcher::Sni(host_patterns(true)?)),
        "sniffer" => {
            let name = m
                .sniffer
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| at("match type `sniffer` needs a `sniffer` name".into()))?;
            Ok(Matcher::Sniffer {
                name: name.to_string(),
                host: host_patterns(false)?,
            })
        }
        other => Err(at(format!(
            "unknown match type {other:?} \
             (always | client_cidr | dst | port | first_bytes | sni | sniffer)"
        ))),
    }
}

pub(crate) fn parse_cache_key_part(rname: &str, s: &str) -> Result<CacheKeyPart, ConfigError> {
    let bad = |m: String| ConfigError::Invalid(format!("resolver {rname}: cache.key: {m}"));
    match s {
        "src_ip" => Ok(CacheKeyPart::SrcIp),
        "src_ip_port" => Ok(CacheKeyPart::SrcIpPort),
        "sni" => Ok(CacheKeyPart::Sni),
        "routing_key" => Ok(CacheKeyPart::RoutingKey),
        _ => {
            let rest = s
                .strip_prefix("first_bytes:")
                .ok_or_else(|| bad(format!("unknown key part {s:?}")))?;
            let (a, b) = rest
                .split_once(':')
                .ok_or_else(|| bad(format!("{s:?} must be `first_bytes:<a>:<b>`")))?;
            let a: usize = a.parse().map_err(|_| bad(format!("bad start in {s:?}")))?;
            let b: usize = b.parse().map_err(|_| bad(format!("bad end in {s:?}")))?;
            if a > b {
                return Err(bad(format!("{s:?}: start > end")));
            }
            if b > PEEK_MAX {
                return Err(bad(format!("{s:?}: end exceeds the {PEEK_MAX}-byte peek")));
            }
            Ok(CacheKeyPart::FirstBytes(a..=b))
        }
    }
}

/// Parse an `sni` host pattern: exact (`eu.example.com`), or a subdomain suffix
/// written `*.eu.example.com` or `.eu.example.com`.
pub(crate) fn parse_host_pattern(p: &str) -> Result<HostPattern, String> {
    let p = p.trim().to_ascii_lowercase();
    if p.is_empty() {
        return Err("empty host pattern".into());
    }
    let body = p.strip_prefix("*.").or_else(|| p.strip_prefix('.'));
    match body {
        Some(rest) => {
            if rest.is_empty() || rest.contains('*') {
                return Err(format!("invalid host pattern {p:?}"));
            }
            Ok(HostPattern::Suffix(format!(".{rest}")))
        }
        None => {
            if p.contains('*') {
                return Err(format!(
                    "invalid host pattern {p:?} (`*` only as `*.suffix`)"
                ));
            }
            Ok(HostPattern::Exact(p))
        }
    }
}

/// Parse a `first_bytes` prefix: `"hex:ffff"` or `"ascii:text"`.
pub(crate) fn parse_byte_spec(s: &str) -> Result<Vec<u8>, String> {
    if let Some(h) = s.strip_prefix("hex:") {
        parse_hex(h)
    } else if let Some(a) = s.strip_prefix("ascii:") {
        Ok(a.as_bytes().to_vec())
    } else {
        Err(format!("prefix {s:?} must start with `hex:` or `ascii:`"))
    }
}

/// Max ports a single `bind: "host:lo-hi"` range may cover — one real socket
/// per port per worker, so an unbounded range risks fd exhaustion from a
/// typo (e.g. `0-65535`).
const MAX_BIND_RANGE: usize = 1024;

/// Parse a listener's `bind` string: either a plain `host:port` socket
/// address, or a `host:lo-hi` port range (requirement F1.4) — one socket per
/// port in the range, all sharing this listener's routes/filters/pool
/// selection (a route's `port` matcher still sees the real accepted/received
/// port). Returns the lowest port's address as the primary bind and the rest
/// (empty for a plain bind) as the extras.
pub(crate) fn parse_bind_spec(
    lname: &str,
    raw: &str,
) -> Result<(SocketAddr, Vec<SocketAddr>), ConfigError> {
    let bad = |s: String| ConfigError::Invalid(format!("listener {lname}: bind: {s}"));
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok((addr, Vec::new()));
    }
    let invalid = || {
        bad(format!(
            "{raw:?} is not a valid socket address or port range"
        ))
    };
    let (host_part, port_part) = raw.rsplit_once(':').ok_or_else(invalid)?;
    let (lo, hi) = port_part.split_once('-').ok_or_else(invalid)?;
    let lo: u16 = lo.trim().parse().map_err(|_| {
        bad(format!(
            "bind range {raw:?} has an invalid lower port bound"
        ))
    })?;
    let hi: u16 = hi.trim().parse().map_err(|_| {
        bad(format!(
            "bind range {raw:?} has an invalid upper port bound"
        ))
    })?;
    if lo == 0 || hi == 0 {
        return Err(bad(format!("bind range {raw:?} includes port 0")));
    }
    if lo > hi {
        return Err(bad(format!("bind range {raw:?} is reversed (lo > hi)")));
    }
    let width = (hi - lo) as usize + 1;
    if width > MAX_BIND_RANGE {
        return Err(bad(format!(
            "bind range {raw:?} covers {width} ports, over the {MAX_BIND_RANGE} limit \
             (one socket per port, per worker)"
        )));
    }
    let host = host_part.trim_start_matches('[').trim_end_matches(']');
    let ip: IpAddr = host
        .parse()
        .map_err(|_| bad(format!("bind range {raw:?} has an invalid host {host:?}")))?;
    let mut addrs = (lo..=hi).map(|p| SocketAddr::new(ip, p));
    let first = addrs
        .next()
        .expect("lo..=hi is non-empty (lo <= hi checked above)");
    Ok((first, addrs.collect()))
}

pub(crate) fn parse_port_range(
    lname: &str,
    i: usize,
    p: &RawPort,
) -> Result<RangeInclusive<u16>, ConfigError> {
    let bad = |s: String| ConfigError::Invalid(format!("listener {lname}: route {i}: {s}"));
    match p {
        RawPort::Single(n) => {
            let n = u16::try_from(*n).map_err(|_| bad(format!("port {n} is out of range")))?;
            if n == 0 {
                return Err(bad("port 0 is not valid".into()));
            }
            Ok(n..=n)
        }
        RawPort::Range(s) => {
            let (lo, hi) = s
                .split_once('-')
                .ok_or_else(|| bad(format!("port range {s:?} must be `lo-hi`")))?;
            let lo: u16 = lo
                .trim()
                .parse()
                .map_err(|_| bad(format!("port range {s:?} has an invalid lower bound")))?;
            let hi: u16 = hi
                .trim()
                .parse()
                .map_err(|_| bad(format!("port range {s:?} has an invalid upper bound")))?;
            if lo == 0 || hi == 0 {
                return Err(bad(format!("port range {s:?} includes port 0")));
            }
            if lo > hi {
                return Err(bad(format!("port range {s:?} is reversed")));
            }
            Ok(lo..=hi)
        }
    }
}

/// Parse a hex string (optional ASCII whitespace between bytes) into bytes.
pub(crate) fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let compact: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if !compact.is_ascii() {
        return Err(format!("non-ASCII character in hex string {s:?}"));
    }
    if !compact.len().is_multiple_of(2) {
        return Err(format!("odd number of hex digits in {s:?}"));
    }
    (0..compact.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&compact[i..i + 2], 16)
                .map_err(|_| format!("invalid hex byte {:?}", &compact[i..i + 2]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_hex;

    #[test]
    fn parse_hex_rejects_non_ascii_without_panicking() {
        // Two chars, four bytes: even length, but byte pairs split a char.
        assert!(parse_hex("\u{37f}\u{37f}").is_err());
        assert!(parse_hex("a\u{37f}b").is_err());
    }

    #[test]
    fn parse_hex_accepts_spaced_bytes() {
        assert_eq!(
            parse_hex("de ad BE ef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }
}
