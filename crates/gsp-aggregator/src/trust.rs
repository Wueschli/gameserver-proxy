//! Which `admin_url`s an ingested payload may carry.
//!
//! `admin_url` is self-reported by whoever pushes to `POST /ingest`, and the
//! fan-out later calls it with the fleet-wide `--instance-token`. Trusting it
//! blindly would turn "can push telemetry" into "can collect the admin token".
//! So every payload's URL is checked at ingest against an [`AdminUrlPolicy`]:
//!
//! - with `--instance-url-allow` entries: the URL's host must match one (an IP
//!   inside a listed CIDR, or a hostname equal to / under a listed
//!   `*.suffix`);
//! - without: the URL's host must be an IP literal equal to the pushing
//!   connection's source address, i.e. an instance can only name itself.
//!
//! Either way the scheme must be `http`/`https`, with a host and no userinfo,
//! query or fragment.

use std::net::IpAddr;

use reqwest::Url;

/// One `--instance-url-allow` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Allow {
    Cidr(IpAddr, u8),
    /// A lower-case hostname, exact.
    Host(String),
    /// `*.example.com`: any host below `example.com` (not the bare domain).
    Suffix(String),
}

impl Allow {
    fn parse(entry: &str) -> Result<Self, String> {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err("empty entry".into());
        }
        if let Some((ip, prefix)) = entry.split_once('/') {
            let ip: IpAddr = ip.parse().map_err(|_| format!("{entry:?}: bad address"))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let prefix: u8 = prefix
                .parse()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("{entry:?}: bad prefix length"))?;
            return Ok(Allow::Cidr(ip, prefix));
        }
        if let Ok(ip) = entry.parse::<IpAddr>() {
            return Ok(Allow::Cidr(ip, if ip.is_ipv4() { 32 } else { 128 }));
        }
        let lower = entry.to_ascii_lowercase();
        match lower.strip_prefix("*.") {
            Some(rest) if !rest.is_empty() && !rest.contains('*') => {
                Ok(Allow::Suffix(format!(".{rest}")))
            }
            _ if lower.contains('*') => Err(format!("{entry:?}: only a leading `*.` is supported")),
            _ => Ok(Allow::Host(lower)),
        }
    }

    fn matches(&self, host: &url::Host<&str>) -> bool {
        match (self, host) {
            (Allow::Cidr(net, prefix), url::Host::Ipv4(ip)) => {
                in_cidr(*net, *prefix, IpAddr::V4(*ip))
            }
            (Allow::Cidr(net, prefix), url::Host::Ipv6(ip)) => {
                in_cidr(*net, *prefix, IpAddr::V6(*ip))
            }
            (Allow::Host(h), url::Host::Domain(d)) => d.eq_ignore_ascii_case(h),
            (Allow::Suffix(s), url::Host::Domain(d)) => d.to_ascii_lowercase().ends_with(s),
            _ => false,
        }
    }
}

fn in_cidr(net: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    fn bits(ip: IpAddr) -> u128 {
        match ip {
            IpAddr::V4(v4) => u128::from(u32::from(v4)),
            IpAddr::V6(v6) => u128::from(v6),
        }
    }
    let width = if net.is_ipv4() { 32u32 } else { 128 };
    if net.is_ipv4() != ip.is_ipv4() {
        return false;
    }
    let shift = width - u32::from(prefix);
    if shift >= 128 {
        return true;
    }
    bits(net) >> shift == bits(ip) >> shift
}

/// The ingest-time rule for `admin_url`; see the module doc.
#[derive(Debug, Clone, Default)]
pub struct AdminUrlPolicy {
    allow: Vec<Allow>,
}

impl AdminUrlPolicy {
    /// Parses the `--instance-url-allow` entries (CIDR, bare IP, hostname or
    /// `*.suffix`). An empty list selects the "host must be the pusher" rule.
    pub fn new<S: AsRef<str>>(entries: &[S]) -> Result<Self, String> {
        let allow = entries
            .iter()
            .map(|e| Allow::parse(e.as_ref()))
            .collect::<Result<_, _>>()?;
        Ok(AdminUrlPolicy { allow })
    }

    /// `Ok` if `admin_url` may be stored for a payload pushed from `peer`
    /// (`None` when the source address is unknown, which only the allowlist
    /// rule can still accept).
    pub fn check(&self, admin_url: &str, peer: Option<IpAddr>) -> Result<(), String> {
        let url = Url::parse(admin_url).map_err(|e| format!("admin_url is not a URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("admin_url must be http or https".into());
        }
        let Some(host) = url.host() else {
            return Err("admin_url has no host".into());
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err("admin_url must not carry credentials".into());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("admin_url must not have a query or fragment".into());
        }
        if self.allow.is_empty() {
            let same = match (&host, peer) {
                (url::Host::Ipv4(h), Some(p)) => unmap(p) == IpAddr::V4(*h),
                (url::Host::Ipv6(h), Some(p)) => unmap(p) == IpAddr::V6(*h),
                _ => false,
            };
            if !same {
                return Err(
                    "admin_url must name the pushing instance's own address (an IP literal \
                     equal to the connection's source); set --instance-url-allow on the \
                     aggregator to accept other hosts"
                        .into(),
                );
            }
        } else if !self.allow.iter().any(|a| a.matches(&host)) {
            return Err("admin_url's host is not covered by --instance-url-allow".into());
        }
        Ok(())
    }
}

/// An IPv4 peer seen through a dual-stack socket arrives as `::ffff:a.b.c.d`.
fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn default_rule_accepts_only_the_pushers_own_ip() {
        let p = AdminUrlPolicy::default();
        assert!(p.check("http://10.0.0.4:9900", ip("10.0.0.4")).is_ok());
        assert!(p
            .check("https://10.0.0.4:9900/gsp-1", ip("10.0.0.4"))
            .is_ok());
        assert!(p.check("http://[::1]:9900", ip("::1")).is_ok());
        assert!(p
            .check("http://10.0.0.4:9900", ip("::ffff:10.0.0.4"))
            .is_ok());
        assert!(p.check("http://10.9.9.9:9900", ip("10.0.0.4")).is_err());
        assert!(p.check("http://evil.example:9900", ip("10.0.0.4")).is_err());
        assert!(p.check("http://10.0.0.4:9900", None).is_err());
    }

    #[test]
    fn allowlist_replaces_the_default_rule() {
        let p =
            AdminUrlPolicy::new(&["10.0.0.0/8", "edge.example", "*.nodes.example", "::1"]).unwrap();
        assert!(p.check("http://10.1.2.3:9900", ip("192.0.2.1")).is_ok());
        assert!(p.check("http://10.1.2.3:9900", None).is_ok());
        assert!(p.check("http://EDGE.example", None).is_ok());
        assert!(p.check("http://a.nodes.example:1", None).is_ok());
        assert!(p.check("http://[::1]:1", None).is_ok());
        assert!(p.check("http://nodes.example", None).is_err());
        assert!(p.check("http://evilnodes.example", None).is_err());
        assert!(p.check("http://11.0.0.1", None).is_err());
        assert!(p.check("http://evil.example", None).is_err());
    }

    #[test]
    fn shape_is_checked_either_way() {
        let p = AdminUrlPolicy::new(&["10.0.0.0/8"]).unwrap();
        for bad in [
            "ftp://10.0.0.1",
            "http://user:pw@10.0.0.1",
            "http://10.0.0.1/?x=1",
            "http://10.0.0.1/#f",
            "10.0.0.1:9900",
            "",
        ] {
            assert!(p.check(bad, ip("10.0.0.1")).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn bad_entries_are_rejected() {
        for bad in ["", "10.0.0.0/33", "a*.example", "*.", "x/y"] {
            assert!(AdminUrlPolicy::new(&[bad]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cidr_edges() {
        assert!(in_cidr(
            "0.0.0.0".parse().unwrap(),
            0,
            "9.9.9.9".parse().unwrap()
        ));
        assert!(in_cidr(
            "10.0.0.1".parse().unwrap(),
            32,
            "10.0.0.1".parse().unwrap()
        ));
        assert!(!in_cidr(
            "10.0.0.1".parse().unwrap(),
            32,
            "10.0.0.2".parse().unwrap()
        ));
        assert!(!in_cidr(
            "10.0.0.0".parse().unwrap(),
            8,
            "::1".parse().unwrap()
        ));
    }
}
